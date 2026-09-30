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
use buck2_core::provider::label::ProvidersName;
use buck2_core::target::label::label::TargetLabel;
use buck2_execute::artifact::artifact_dyn::ArtifactDyn;
use buck2_execute::artifact::fs::ExecutorFs;
use buck2_hash::BuckMutMap;
use buck2_node::nodes::frontend::TargetGraphCalculation;
use buck2_repl_syntax::text::CappedString;
use buck2_server_ctx::ctx::ServerCommandContextTrait;
use buck2_server_ctx::target_resolution_config::TargetResolutionConfig;
use dice::DiceComputations;
use dupe::Dupe;
use futures::FutureExt;

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
    let labels = resolve(sctx, dc, target_cfg, &spec.patterns)
        .await
        .map_err(|e| ReplFailure::from_buck2(repl_error::Kind::Buck, &e))?;
    if let Some(run) = &spec.run {
        if labels.len() != 1 {
            return Err(ReplFailure::new(
                repl_error::Kind::Usage,
                &format_args!(
                    "`:run` needs exactly one target, but {} {}",
                    spec.patterns.join(" "),
                    match labels.len() {
                        0 => "matches none".to_owned(),
                        n => format!("matches {n}"),
                    }
                ),
            ));
        }
        let result = build_labels(dc, labels, true).await?;
        run_command(dc, &result, run).await
    } else {
        let result = build_labels(dc, labels, false).await?;
        let artifact_fs = dc
            .get_artifact_fs()
            .await
            .map_err(|e| ReplFailure::from_buck2(repl_error::Kind::Buck, &e))?;
        list_outputs(&result, artifact_fs)
    }
}

/// The configured targets that the patterns match, each with whether it may be skipped when it
/// is incompatible with its configuration: as in `buck2 build`, targets named explicitly may
/// not, targets matched by a wildcard (`//pkg:`, `//pkg/...`) may.
async fn resolve(
    sctx: &dyn ServerCommandContextTrait,
    dc: &mut DiceComputations<'_>,
    target_cfg: &TargetCfg,
    patterns: &[String],
) -> buck2_error::Result<BTreeMap<ConfiguredProvidersLabel, bool>> {
    let resolved =
        parse_and_resolve_patterns_with_modifiers_from_cli_args::<ProvidersPatternExtra>(
            dc,
            patterns,
            sctx.working_dir(),
        )
        .await?;
    let mut labels: Vec<(ProvidersLabelWithModifiers, bool)> = Vec::new();
    for (package_with_modifiers, spec) in resolved.specs {
        let PackageLabelWithModifiers { package, modifiers } = package_with_modifiers;
        match spec {
            PackageSpec::Targets(targets) => {
                for (name, extra) in targets {
                    labels.push((
                        ProvidersLabelWithModifiers {
                            providers_label: ProvidersLabel::new(
                                TargetLabel::new(package.dupe(), name.as_ref()),
                                extra.providers,
                            ),
                            modifiers: modifiers.dupe(),
                        },
                        false,
                    ));
                }
            }
            PackageSpec::All() => {
                let results = dc.get_interpreter_results(package.dupe()).await?;
                for name in results.targets().keys() {
                    labels.push((
                        ProvidersLabelWithModifiers {
                            providers_label: ProvidersLabel::new(
                                TargetLabel::new(package.dupe(), name),
                                ProvidersName::Default,
                            ),
                            modifiers: modifiers.dupe(),
                        },
                        true,
                    ));
                }
            }
        }
    }

    let config = TargetResolutionConfig::from_args(dc, target_cfg, sctx, &[]).await?;
    let mut configured: BTreeMap<ConfiguredProvidersLabel, bool> = BTreeMap::new();
    for (label, skippable) in labels {
        for label in config
            .get_configured_provider_label_with_modifiers(dc, &label)
            .await?
        {
            // A target named explicitly is never skipped, even if a wildcard matches it too.
            let entry = configured.entry(label).or_insert(skippable);
            *entry = *entry && skippable;
        }
    }
    Ok(configured)
}

/// Builds the targets and materializes their outputs (their `RunInfo` for `:run`, their default
/// outputs otherwise), failing if anything failed.
async fn build_labels(
    dc: &mut DiceComputations<'_>,
    labels: BTreeMap<ConfiguredProvidersLabel, bool>,
    run: bool,
) -> Result<BuildTargetResult, ReplFailure> {
    let (builder, consumer) = AsyncBuildTargetResultBuilder::new(None, Instant::now());
    let consumer = &consumer;
    let result = builder
        .wait_for(
            false,
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

    let mut errors: Vec<&buck2_error::Error> = Vec::new();
    for target in result.configured.values().flatten() {
        errors.extend(target.errors.iter().map(|e| &e.inner));
        errors.extend(target.outputs.iter().filter_map(|o| o.inner.as_ref().err()));
    }
    errors.extend(result.other_errors.values().flatten());
    match errors.as_slice() {
        [] => Ok(result),
        [e] => Err(ReplFailure::from_buck2(repl_error::Kind::Buck, e)),
        errors => Err(ReplFailure::new(
            repl_error::Kind::Buck,
            &BuildErrors(errors),
        )),
    }
}

/// The errors of a failed build, one after the other.
struct BuildErrors<'a>(&'a [&'a buck2_error::Error]);

impl fmt::Display for BuildErrors<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the build failed with {} errors", self.0.len())?;
        for e in self.0 {
            write!(f, "\n\n{e:?}")?;
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
    let Some((label, Some(target))) = result.configured.iter().next() else {
        // An explicitly named target is never skipped: its incompatibility is an error.
        return Err(ReplFailure::new(
            repl_error::Kind::Internal,
            &"the target of `:run` was not built",
        ));
    };
    let label = label.unconfigured().to_string();
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
