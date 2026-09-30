/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! `:build` and `:run`: native builds in the request's transaction, as `buck2 build` and
//! `buck2 run` do them (no Starlark runs).
//!
//! Only DICE-level functions are called here: the request already holds its transaction, and
//! the server's build command would take another one (never nest `with_dice_ctx`).

use std::cell::Cell;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fmt;
use std::time::Instant;

use buck2_artifact::artifact::artifact_type::Artifact;
use buck2_build_api::actions::artifact::get_artifact_fs::GetArtifactFs;
use buck2_build_api::build::AsyncBuildTargetResultBuilder;
use buck2_build_api::build::BuildConfiguredLabelOptions;
use buck2_build_api::build::BuildProviderType;
use buck2_build_api::build::BuildTargetResult;
use buck2_build_api::build::ConfiguredBuildTargetResult;
use buck2_build_api::build::ProvidersToBuild;
use buck2_build_api::build::build_configured_label;
use buck2_build_api::interpreter::rule_defs::cmd_args::ArtifactPathMapper;
use buck2_build_api::interpreter::rule_defs::cmd_args::CommandLineArgLike;
use buck2_build_api::interpreter::rule_defs::cmd_args::CommandLineBuilder;
use buck2_build_api::materialize::MaterializationAndUploadContext;
use buck2_cli_proto::ReplRun;
use buck2_cli_proto::TargetCfg;
use buck2_cli_proto::repl_error;
use buck2_common::pattern::parse_from_cli::parse_and_resolve_patterns_with_modifiers_from_cli_args;
use buck2_core::content_hash::ContentBasedPathHash;
use buck2_core::execution_types::executor_config::PathSeparatorKind;
use buck2_core::fs::artifact_path_resolver::ArtifactFs;
use buck2_core::package::PackageLabelWithModifiers;
use buck2_core::pattern::pattern::PackageSpec;
use buck2_core::pattern::pattern::ProvidersLabelWithModifiers;
use buck2_core::pattern::pattern_type::ProvidersPatternExtra;
use buck2_core::provider::label::ConfiguredProvidersLabel;
use buck2_core::provider::label::ProvidersLabel;
use buck2_events::dispatch::console_message;
use buck2_execute::artifact::artifact_dyn::ArtifactDyn;
use buck2_execute::artifact::fs::ExecutorFs;
use buck2_hash::BuckMutMap;
use buck2_node::load_patterns::MissingTargetBehavior;
use buck2_node::nodes::frontend::TargetGraphCalculation;
use buck2_repl_syntax::text::CappedString;
use buck2_server_ctx::ctx::ServerCommandContextTrait;
use buck2_server_ctx::target_resolution_config::TargetResolutionConfig;
use dice::DiceComputations;
use dupe::Dupe;
use futures::FutureExt;
use starlark_map::small_set::SmallSet;

use crate::repl::render::MAX_TEXT_BYTES;
use crate::repl::render::ReplFailure;

/// Room for the framing of each argument of a `ReplRun` (INV-13).
const ARG_OVERHEAD: usize = 8;

/// `:build <pattern>...` or `:run [--print] <target> [-- args...]`.
#[derive(Clone, Debug)]
pub(crate) struct BuildSpec {
    /// The target patterns, as typed (relative to the session's directory).
    pub(crate) patterns: Vec<String>,
    /// For `:run`: what to do with the target once it is built.
    pub(crate) run: Option<RunSpec>,
}

#[derive(Clone, Debug)]
pub(crate) struct RunSpec {
    /// Arguments for the program (after `--`).
    pub(crate) args: Vec<String>,
    /// `--print`: the client only prints the command.
    pub(crate) print: bool,
}

/// What a build produced.
pub(crate) enum Built {
    /// `:build`: the outputs of each target.
    Outputs {
        /// One `label  path` line per output (a target without outputs gets a line with its
        /// label only), cut at [`MAX_TEXT_BYTES`].
        listing: String,
        truncated: bool,
        /// `{label: [paths]}`, which becomes `_`.
        value: serde_json::Value,
    },
    /// `:run`: the command that runs the target.
    Run(ReplRun),
}

/// Builds the targets of `spec` (and, for `:run`, resolves the command line of the one target).
/// Dropping the future stops the build: it is only DICE work.
pub(crate) async fn build(
    sctx: &dyn ServerCommandContextTrait,
    dc: &mut DiceComputations<'_>,
    target_cfg: &TargetCfg,
    spec: &BuildSpec,
) -> Result<Built, ReplFailure> {
    let opts = TargetOptions::new(sctx);
    let resolved = resolve(sctx, dc, target_cfg, &spec.patterns, opts)
        .await
        .map_err(|e| ReplFailure::from_buck2(repl_error::Kind::Buck, &e))?;
    if let Some(run) = &spec.run {
        let mut errors = BuildErrors::default();
        for e in &resolved.errors {
            errors.add(e);
        }
        errors.check()?;
        if resolved.labels.len() != 1 {
            return Err(ReplFailure::new(
                repl_error::Kind::Usage,
                &format_args!(
                    "`:run` needs exactly one target, but {} {}",
                    spec.patterns.join(" "),
                    match resolved.labels.len() {
                        0 => "matches none".to_owned(),
                        n => format!("matches {n}"),
                    }
                ),
            ));
        }
        // The target to run was asked for, even if a wildcard matched it (as `//pkg:` does when
        // `pkg` has one target): if it is incompatible, that is an error.
        let labels = resolved
            .labels
            .into_keys()
            .map(|label| (label, false))
            .collect();
        let result = build_labels(dc, labels, Vec::new(), false, true).await?;
        run_command(dc, &result, run).await
    } else {
        let Resolved { labels, errors } = resolved;
        let result = build_labels(dc, labels, errors, opts.fail_fast, false).await?;
        let artifact_fs = dc
            .get_artifact_fs()
            .await
            .map_err(|e| ReplFailure::from_buck2(repl_error::Kind::Buck, &e))?;
        list_outputs(&result, artifact_fs)
    }
}

/// The options of `buck2 build` about which targets to build and when to stop, from the
/// session's build options (the others act through the transaction).
#[derive(Clone, Copy)]
struct TargetOptions {
    /// `--skip-missing-targets`.
    missing: MissingTargetBehavior,
    /// `--skip-incompatible-targets`: targets named explicitly may be skipped too.
    skip_incompatible: bool,
    /// `--fail-fast`.
    fail_fast: bool,
}

impl TargetOptions {
    fn new(sctx: &dyn ServerCommandContextTrait) -> Self {
        let opts = sctx.build_options();
        TargetOptions {
            missing: MissingTargetBehavior::from_skip(opts.is_some_and(|o| o.skip_missing_targets)),
            skip_incompatible: opts.is_some_and(|o| o.skip_incompatible_targets),
            fail_fast: opts.is_some_and(|o| o.fail_fast),
        }
    }
}

/// What target patterns resolve to.
#[derive(Default)]
struct Resolved {
    /// The configured targets, each with whether it may be skipped when it is incompatible with
    /// its configuration.
    labels: BTreeMap<ConfiguredProvidersLabel, bool>,
    /// Packages that failed to load, missing targets and targets that failed to configure: as
    /// `buck2 build`, `:build` builds the other targets and then fails with these errors.
    errors: Vec<buck2_error::Error>,
}

impl Resolved {
    fn add_label(&mut self, label: ConfiguredProvidersLabel, skippable: bool) {
        // Patterns with different modifiers may configure to the same label, which is built
        // once: it may be skipped only if each of them allows it.
        let entry = self.labels.entry(label).or_insert(skippable);
        *entry = *entry && skippable;
    }
}

/// Resolves the patterns and configures the targets they match, loading the packages and
/// configuring the targets concurrently.
async fn resolve(
    sctx: &dyn ServerCommandContextTrait,
    dc: &mut DiceComputations<'_>,
    target_cfg: &TargetCfg,
    patterns: &[String],
    opts: TargetOptions,
) -> buck2_error::Result<Resolved> {
    let resolved =
        parse_and_resolve_patterns_with_modifiers_from_cli_args::<ProvidersPatternExtra>(
            dc,
            patterns,
            sctx.working_dir(),
        )
        .await?;
    let config = TargetResolutionConfig::from_args(dc, target_cfg, sctx, &[]).await?;
    let config = &config;
    let packages = dc
        .compute_join(resolved.specs, async |dc, (package, spec)| {
            resolve_package(dc, config, package, spec, opts).await
        })
        .await;
    let mut all = Resolved::default();
    for package in packages {
        for (label, skippable) in package.labels {
            all.add_label(label, skippable);
        }
        all.errors.extend(package.errors);
    }
    Ok(all)
}

/// The targets of one package that a pattern matches, configured. As in `buck2 build`, the
/// targets matched by a wildcard (`//pkg:`, `//pkg/...`) may be skipped when they are
/// incompatible, the targets named explicitly only with `--skip-incompatible-targets`. A target
/// named in a package that a wildcard also matches is merged into the wildcard when the patterns
/// are resolved, so it may be skipped too (as in `buck2 build //pkg:x //pkg:`).
async fn resolve_package(
    dc: &mut DiceComputations<'_>,
    config: &TargetResolutionConfig,
    package: PackageLabelWithModifiers,
    spec: PackageSpec<ProvidersPatternExtra>,
    opts: TargetOptions,
) -> Resolved {
    let skippable = match spec {
        PackageSpec::Targets(_) => opts.skip_incompatible,
        PackageSpec::All() => true,
    };
    let PackageLabelWithModifiers { package, modifiers } = package;
    let mut resolved = Resolved::default();
    let results = match dc.get_interpreter_results(package.dupe()).await {
        Ok(results) => results,
        Err(e) => {
            resolved.errors.push(e);
            return resolved;
        }
    };
    let (targets, missing) = results.apply_spec(spec);
    if let Some(missing) = missing {
        match opts.missing {
            MissingTargetBehavior::Fail => resolved
                .errors
                .extend(missing.into_all_errors().map(buck2_error::Error::from)),
            MissingTargetBehavior::Warn => console_message(missing.missing_targets_warning()),
        }
    }
    let modifiers = &modifiers;
    let configured = dc
        .compute_join(targets, async |dc, ((_name, extra), node)| {
            let label = ProvidersLabelWithModifiers {
                providers_label: ProvidersLabel::new(node.label().dupe(), extra.providers),
                modifiers: modifiers.dupe(),
            };
            config
                .get_configured_provider_label_with_modifiers(dc, &label)
                .await
        })
        .await;
    for configured in configured {
        match configured {
            Ok(labels) => {
                for label in labels {
                    resolved.add_label(label, skippable);
                }
            }
            Err(e) => resolved.errors.push(e),
        }
    }
    resolved
}

/// Builds the targets and materializes their outputs (their `RunInfo` for `:run`, their default
/// outputs otherwise). Fails with `errors` (those of resolving the targets) and the errors of the
/// build, if there are any: with `fail_fast`, nothing is built when `errors` is not empty, and
/// the build stops at its first error.
async fn build_labels(
    dc: &mut DiceComputations<'_>,
    labels: BTreeMap<ConfiguredProvidersLabel, bool>,
    errors: Vec<buck2_error::Error>,
    fail_fast: bool,
    run: bool,
) -> Result<BuildTargetResult, ReplFailure> {
    let labels = if fail_fast && !errors.is_empty() {
        BTreeMap::new()
    } else {
        labels
    };
    let (builder, consumer) = AsyncBuildTargetResultBuilder::new(None, Instant::now());
    let consumer = &consumer;
    let result = builder
        .wait_for(
            fail_fast,
            dc.compute_join(labels, async |ctx, (label, skippable)| {
                let consumer = consumer.clone();
                ctx.with_linear_recompute(|ctx| {
                    async move {
                        build_configured_label(
                            &consumer,
                            ctx,
                            MaterializationAndUploadContext::materialize(),
                            label,
                            &ProvidersToBuild {
                                default: !run,
                                default_other: !run,
                                run,
                                tests: false,
                            },
                            BuildConfiguredLabelOptions {
                                skippable,
                                graph_properties: Default::default(),
                                return_run_args: run,
                            },
                            None,
                        )
                        .await
                    }
                    .boxed()
                })
                .await
            })
            .map(|_: Vec<()>| ()),
        )
        .await
        .map_err(|e| ReplFailure::from_buck2(repl_error::Kind::Buck, &e))?;

    let mut all = BuildErrors::default();
    for e in &errors {
        all.add(e);
    }
    for target in result.configured.values().flatten() {
        for e in &target.errors {
            all.add(&e.inner);
        }
        for e in target.outputs.iter().filter_map(|o| o.inner.as_ref().err()) {
            all.add(e);
        }
    }
    for e in result.other_errors.values().flatten() {
        all.add(e);
    }
    all.check()?;
    Ok(result)
}

/// The distinct errors of a build (as `buck2 build` reports them, an error that several targets
/// share, such as that of an action they depend on, is shown once).
#[derive(Default)]
struct BuildErrors(SmallSet<String>);

impl BuildErrors {
    fn add(&mut self, e: &buck2_error::Error) {
        self.0.insert(format!("{e:?}"));
    }

    /// Fails if there are errors: one error is rendered like other commands' errors, several one
    /// after the other.
    fn check(self) -> Result<(), ReplFailure> {
        let errors: Vec<String> = self.0.into_iter().collect();
        match errors.as_slice() {
            [] => Ok(()),
            [e] => Err(ReplFailure::new(repl_error::Kind::Buck, e)),
            errors => Err(ReplFailure::new(repl_error::Kind::Buck, &AllErrors(errors))),
        }
    }
}

/// Several errors, one after the other.
struct AllErrors<'a>(&'a [String]);

impl fmt::Display for AllErrors<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the build failed with {} errors", self.0.len())?;
        for e in self.0 {
            write!(f, "\n\n{e}")?;
        }
        Ok(())
    }
}

/// The default outputs of each target: a listing, and `{label: [paths]}`. Paths are relative to
/// the project root, as `buck2 build --show-output` prints them.
fn list_outputs(
    result: &BuildTargetResult,
    artifact_fs: &ArtifactFs,
) -> Result<Built, ReplFailure> {
    let mut listing = CappedString::new(MAX_TEXT_BYTES);
    // The same target in two configurations (e.g. `//:x?a` and `//:x?b`) has one entry.
    let mut by_label: BTreeMap<String, (Vec<String>, HashSet<String>)> = BTreeMap::new();
    for (label, target) in &result.configured {
        // `None`: skipped, as incompatible with its configuration.
        let Some(target) = target else {
            continue;
        };
        let label = label.unconfigured().to_string();
        let (paths, seen) = by_label.entry(label.clone()).or_default();
        let mut listed = false;
        for output in target.outputs.iter().filter_map(|o| o.inner.as_ref().ok()) {
            if !matches!(output.provider_type, BuildProviderType::Default) {
                continue;
            }
            for (artifact, _) in output.values.iter() {
                let path = artifact
                    .resolve_configuration_hash_path(artifact_fs)
                    .map_err(|e| ReplFailure::from_buck2(repl_error::Kind::Buck, &e))?
                    .to_string();
                if seen.insert(path.clone()) {
                    // A `CappedString` never fails: it only stops at its cap.
                    let _ignored = fmt::write(&mut listing, format_args!("{label}  {path}\n"));
                    listed = true;
                    paths.push(path);
                }
            }
        }
        if !listed && paths.is_empty() {
            let _ignored = fmt::write(&mut listing, format_args!("{label}\n"));
        }
    }
    let truncated = listing.truncated();
    let listing = if by_label.is_empty() {
        // Nothing matched, or every target was skipped: `_` is an empty dict.
        "{}".to_owned()
    } else {
        listing.into_string().trim_end_matches('\n').to_owned()
    };
    let value = by_label
        .into_iter()
        .map(|(label, (paths, _))| {
            let paths = paths.into_iter().map(serde_json::Value::String).collect();
            (label, serde_json::Value::Array(paths))
        })
        .collect();
    Ok(Built::Outputs {
        listing,
        truncated,
        value: serde_json::Value::Object(value),
    })
}

/// The command line that runs the one target of `result`, as `buck2 run` makes it: absolute
/// paths, then the user's arguments.
async fn run_command(
    dc: &mut DiceComputations<'_>,
    result: &BuildTargetResult,
    run: &RunSpec,
) -> Result<Built, ReplFailure> {
    let Some((label, target)) = result.configured.iter().next() else {
        return Err(ReplFailure::new(
            repl_error::Kind::Internal,
            &"the target of `:run` was not built",
        ));
    };
    let label = label.unconfigured().to_string();
    let Some(target) = target else {
        // Not expected: the target is built as not skippable, so that its incompatibility with
        // its configuration is an error of the build.
        return Err(ReplFailure::new(
            repl_error::Kind::Buck,
            &format_args!("target `{label}` is incompatible with its configuration"),
        ));
    };
    let not_runnable = || {
        ReplFailure::new(
            repl_error::Kind::Buck,
            &format_args!(
                "target `{label}` is not a binary rule: it has no `RunInfo` provider to run"
            ),
        )
    };
    let artifact_fs = dc
        .get_artifact_fs()
        .await
        .map_err(|e| ReplFailure::from_buck2(repl_error::Kind::Buck, &e))?;
    let mut argv = command_line(target, artifact_fs)
        .map_err(|e| ReplFailure::from_buck2(repl_error::Kind::Buck, &e))?
        .ok_or_else(not_runnable)?;
    if argv.is_empty() {
        return Err(not_runnable());
    }
    argv.extend(run.args.iter().cloned());

    // The command goes to the client in one message (INV-13).
    let size = argv.iter().map(|a| a.len() + ARG_OVERHEAD).sum::<usize>() + label.len();
    if size > MAX_TEXT_BYTES {
        return Err(ReplFailure::new(
            repl_error::Kind::Unsupported,
            &format_args!(
                "the command line of `{label}` is too long for the repl ({} KiB, at most {} \
                 KiB); run it with `buck2 run {label}`",
                size >> 10,
                MAX_TEXT_BYTES >> 10
            ),
        ));
    }
    Ok(Built::Run(ReplRun {
        argv,
        // The client's directory, as `buck2 run`.
        cwd: String::new(),
        label,
        print_only: run.print,
    }))
}

/// The `RunInfo` command line of a built target, with absolute paths to run it from anywhere.
/// `None` if the target has no `RunInfo`. Copied from the build result report of `buck2 build`.
fn command_line(
    target: &ConfiguredBuildTargetResult,
    artifact_fs: &ArtifactFs,
) -> buck2_error::Result<Option<Vec<String>>> {
    let Some(run_info) = target.run_info.as_ref() else {
        return Ok(None);
    };
    // Content-based paths need the hash of the artifacts that were built.
    let mut hashes = BuckMutMap::default();
    for output in target.outputs.iter().filter_map(|o| o.inner.as_ref().ok()) {
        if matches!(output.provider_type, BuildProviderType::Run) {
            for (artifact, value) in output.values.iter() {
                hashes.insert(artifact, value.content_based_path_hash());
            }
        }
    }
    let path_separator = if cfg!(windows) {
        PathSeparatorKind::Windows
    } else {
        PathSeparatorKind::Unix
    };
    let executor_fs = ExecutorFs::new(artifact_fs, path_separator);
    let mapper = HashMapper {
        hashes,
        missing: Cell::new(0),
        scratch: ContentBasedPathHash::Scratch,
    };
    let mut argv = Vec::<String>::new();
    let mut builder =
        CommandLineBuilder::new_with_options(&mut argv, &mapper, &executor_fs, true, None);
    run_info
        .as_ref()
        .value()
        .as_ref()
        .add_to_command_line(&mut builder)?;
    if mapper.missing.get() > 0 {
        return Err(buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "the command line depends on content-based outputs that were not built"
        ));
    }
    Ok(Some(argv))
}

/// The content hashes of the built artifacts, counting the paths that need one it does not have.
struct HashMapper<'a> {
    hashes: BuckMutMap<&'a Artifact, ContentBasedPathHash>,
    missing: Cell<usize>,
    scratch: ContentBasedPathHash,
}

impl ArtifactPathMapper for HashMapper<'_> {
    fn get(&self, artifact: &Artifact) -> Option<&ContentBasedPathHash> {
        let hash = self.hashes.get(artifact);
        if artifact.path_resolution_requires_artifact_value() && hash.is_none() {
            self.missing.set(self.missing.get() + 1);
            // Path resolution must succeed; the command line is refused afterwards.
            return Some(&self.scratch);
        }
        hash
    }
}
