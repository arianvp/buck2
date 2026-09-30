/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! `:info <target>`, `:ls [package]` and the hidden `:__locate <target|module>`: what the target
//! graph says about targets, read from DICE in the request's transaction (no Starlark runs).

use std::fmt;
use std::fmt::Write;

use buck2_cli_proto::repl_error;
use buck2_common::dice::cells::HasCellResolver;
use buck2_common::file_ops::dice::DiceFileComputations;
use buck2_common::package_listing::dice::DicePackageListingResolver;
use buck2_common::pattern::parse_from_cli::parse_and_resolve_patterns_from_cli_args;
use buck2_common::pattern::parse_from_cli::parse_patterns_from_cli_args;
use buck2_core::build_file_path::BuildFilePath;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::cell_path::CellPathRef;
use buck2_core::pattern::pattern::ParsedPattern;
use buck2_core::pattern::pattern_type::TargetPatternExtra;
use buck2_core::target::label::label::TargetLabel;
use buck2_interpreter::paths::path::OwnedStarlarkPath;
use buck2_interpreter::paths::path::StarlarkPath;
use buck2_interpreter_for_build::interpreter::dice_calculation_delegate::HasCalculationDelegate;
use buck2_node::attrs::display::AttrDisplayWithContextExt;
use buck2_node::attrs::fmt_context::AttrFmtContext;
use buck2_node::attrs::inspect_options::AttrInspectOptions;
use buck2_node::configuration::calculation::CONFIGURATION_CALCULATION;
use buck2_node::nodes::frontend::TargetGraphCalculation;
use buck2_node::nodes::unconfigured::TargetNode;
use buck2_query::query::environment::AttrFmtOptions;
use buck2_repl_syntax::text::CappedString;
use buck2_repl_syntax::text::find_name_line;
use buck2_server_ctx::ctx::ServerCommandContextTrait;
use dice::DiceComputations;
use dupe::Dupe;

use crate::repl::render::MAX_STREAM_BYTES;
use crate::repl::render::MAX_TEXT_BYTES;
use crate::repl::render::ReplFailure;
use crate::repl::session::repl_file;

/// Longest rendering of one attribute value by `:info`.
const MAX_ATTR_BYTES: usize = 1 << 10;

/// Most deps `:info` names (it counts them all).
const MAX_DEPS_SHOWN: usize = 20;

/// Longest warning of `:__locate` (with the error that made it).
const MAX_WARNING_BYTES: usize = 8 << 10;

/// What `:info`, `:ls` or `:__locate` looks at.
#[derive(Clone, Debug)]
pub(crate) enum InspectSpec {
    /// `:info <target>`.
    Info { target: String },
    /// `:ls [package]`: the targets matched by a pattern (`""` for the session's package).
    Ls { package: String },
    /// `:__locate <target|module>`: where to edit a target (its build file, at the line where
    /// it is defined if known) or a module to load (`//pkg:x.bzl`).
    Locate { what: String },
    /// `:set target_platforms <target>`: the target must exist.
    Platform { target: String },
}

/// What was found.
pub(crate) enum Inspected {
    /// Text for the client's stdout.
    Text(String),
    /// `:__locate`: `{"path": <absolute path>, "line": <line or null>, "module": <bool>}`, and
    /// `"warning"` when the location is not the one asked for.
    Location(String),
    /// The target platform, as an absolute label.
    Platform(String),
}

fn buck(e: buck2_error::Error) -> ReplFailure {
    ReplFailure::from_buck2(repl_error::Kind::Buck, &e)
}

fn usage(message: &dyn fmt::Display) -> ReplFailure {
    ReplFailure::new(repl_error::Kind::Usage, message)
}

/// Looks at the target graph. Dropping the future stops the work: it is only DICE work.
pub(crate) async fn inspect(
    sctx: &dyn ServerCommandContextTrait,
    dc: &mut DiceComputations<'_>,
    spec: &InspectSpec,
) -> Result<Inspected, ReplFailure> {
    match spec {
        InspectSpec::Info { target } => {
            let node = target_node(sctx, dc, target, ":info").await?;
            Ok(Inspected::Text(info(sctx, dc, &node).await?))
        }
        InspectSpec::Ls { package } => Ok(Inspected::Text(ls(sctx, dc, package).await?)),
        InspectSpec::Locate { what } => {
            let location = if is_module(what) {
                module_location(sctx, dc, what).await?
            } else {
                target_edit_location(sctx, dc, what).await?
            };
            let mut json = serde_json::json!({
                "path": location.path,
                "line": location.line,
                "module": location.module,
            });
            if let (Some(warning), Some(json)) = (location.warning, json.as_object_mut()) {
                json.insert("warning".to_owned(), warning.into());
            }
            Ok(Inspected::Location(json.to_string()))
        }
        InspectSpec::Platform { target } => {
            // Relative to the session's directory, as everything is in the session; kept as an
            // absolute label, as `--target-platforms` takes it.
            let label = target_label(sctx, dc, target, ":set target_platforms").await?;
            dc.get_target_node(&label).await.map_err(buck)?;
            // A `platform()` target, as `--target-platforms` needs (it is analysed for its
            // `PlatformInfo`): anything else would break every request that configures.
            let checked = match CONFIGURATION_CALCULATION.get() {
                Ok(calculation) => calculation.get_platform_configuration(dc, &label).await,
                Err(e) => Err(e),
            };
            if let Err(e) = checked {
                let mut failure = buck(e);
                failure.add_note(&format_args!(
                    "`{label}` is not a target platform; the setting is unchanged"
                ));
                return Err(failure);
            }
            Ok(Inspected::Platform(label.to_string()))
        }
    }
}

/// Whether `what` names a module to load (`//pkg:x.bzl`) rather than a target.
fn is_module(what: &str) -> bool {
    [".bzl", ".bxl"].iter().any(|e| what.ends_with(e))
}

/// The one target `target` names (relative to the session's directory).
async fn target_label(
    sctx: &dyn ServerCommandContextTrait,
    dc: &mut DiceComputations<'_>,
    target: &str,
    command: &str,
) -> Result<TargetLabel, ReplFailure> {
    let patterns = parse_patterns_from_cli_args::<TargetPatternExtra>(
        dc,
        &[target.to_owned()],
        sctx.working_dir(),
    )
    .await
    .map_err(buck)?;
    match patterns.as_slice() {
        [ParsedPattern::Target(package, name, TargetPatternExtra)] => {
            Ok(TargetLabel::new(package.dupe(), name.as_ref()))
        }
        _ => Err(usage(&format_args!(
            "`{command}` takes one target, not a pattern: `{target}` may match several"
        ))),
    }
}

/// The node of the one target `target` names (relative to the session's directory).
async fn target_node(
    sctx: &dyn ServerCommandContextTrait,
    dc: &mut DiceComputations<'_>,
    target: &str,
    command: &str,
) -> Result<TargetNode, ReplFailure> {
    let label = target_label(sctx, dc, target, command).await?;
    dc.get_target_node(&label).await.map_err(buck)
}

/// Where `:edit <target>` edits: the target's build file, at the line where it is defined. A
/// package that does not load has no targets, and that is when one most wants to edit its build
/// file: it is edited then (from the top), with a warning that says why.
async fn target_edit_location(
    sctx: &dyn ServerCommandContextTrait,
    dc: &mut DiceComputations<'_>,
    target: &str,
) -> Result<Location, ReplFailure> {
    let label = target_label(sctx, dc, target, ":edit").await?;
    let error = match dc.get_target_node(&label).await {
        Ok(node) => return target_location(sctx, dc, &node).await,
        Err(e) => e,
    };
    let package = label.pkg();
    if dc.get_interpreter_results(package.dupe()).await.is_ok() {
        // The package loads: the target is not in it.
        return Err(buck(error));
    }
    let Ok(listing) = DicePackageListingResolver(dc)
        .resolve_package_listing(package.dupe())
        .await
    else {
        // No build file either.
        return Err(buck(error));
    };
    let build_file = BuildFilePath::new(package.dupe(), listing.buildfile().to_owned()).path();
    let (path, shown) = file_paths(sctx, dc, build_file.as_ref()).await?;
    let mut warning = CappedString::new(MAX_WARNING_BYTES);
    // A `CappedString` never fails.
    let _ignored = write!(
        warning,
        "`{package}` does not load, so `{shown}` is opened at the top:\n{error:?}"
    );
    let warning = with_cut_note(warning);
    Ok(Location {
        path,
        shown,
        line: None,
        module: false,
        warning: Some(warning),
    })
}

/// Where something is, for an editor.
struct Location {
    /// Absolute.
    path: String,
    /// Relative to the project root, for people.
    shown: String,
    /// 1-based.
    line: Option<usize>,
    /// A module to load (reloaded after it is edited, if it is loaded), not a build file.
    module: bool,
    /// Why the location is not the one asked for.
    warning: Option<String>,
}

/// The file of `path`: its absolute path and its path relative to the project root.
async fn file_paths(
    sctx: &dyn ServerCommandContextTrait,
    dc: &mut DiceComputations<'_>,
    path: CellPathRef<'_>,
) -> Result<(String, String), ReplFailure> {
    let relative = dc
        .get_cell_resolver()
        .await
        .map_err(buck)?
        .resolve_path(path)
        .map_err(buck)?;
    let absolute = sctx.project_root().resolve(&relative);
    Ok((absolute.to_string(), relative.to_string()))
}

/// The build file of a target, at the line where the target is defined: as buck2 recorded it
/// (with `--target-call-stacks`), else the first line with `name = "<target>"`.
async fn target_location(
    sctx: &dyn ServerCommandContextTrait,
    dc: &mut DiceComputations<'_>,
    node: &TargetNode,
) -> Result<Location, ReplFailure> {
    let build_file: CellPath = node.buildfile_path().path();
    let (path, shown) = file_paths(sctx, dc, build_file.as_ref()).await?;
    let line = match node.root_location() {
        Some(root) => Some(root.line.saturating_add(1)),
        None => DiceFileComputations::read_file_if_exists(dc, build_file.as_ref())
            .await
            .ok()
            .flatten()
            .and_then(|content| find_name_line(&content, node.label().name().as_str())),
    };
    Ok(Location {
        path,
        shown,
        line,
        module: false,
        warning: None,
    })
}

/// The file of a module, named as `load()` at the prompt names it (`//pkg:x.bzl`, `:x.bzl`).
async fn module_location(
    sctx: &dyn ServerCommandContextTrait,
    dc: &mut DiceComputations<'_>,
    module: &str,
) -> Result<Location, ReplFailure> {
    let cwd = dc
        .get_cell_resolver()
        .await
        .map_err(buck)?
        .get_cell_path(sctx.working_dir());
    let repl_path = repl_file(&cwd).map_err(buck)?;
    let resolved = {
        let calc = dc
            .get_interpreter_calculator(OwnedStarlarkPath::BxlFile(repl_path.clone()))
            .await
            .map_err(buck)?;
        calc.resolve_load(StarlarkPath::BxlFile(&repl_path), module)
            .await
            .map_err(buck)?
    };
    let (path, shown) = file_paths(sctx, dc, resolved.path()).await?;
    Ok(Location {
        path,
        shown,
        line: None,
        module: true,
        warning: None,
    })
}

/// `:info <target>`: its label, rule type, build file, the attributes set, and its deps.
async fn info(
    sctx: &dyn ServerCommandContextTrait,
    dc: &mut DiceComputations<'_>,
    node: &TargetNode,
) -> Result<String, ReplFailure> {
    let location = target_location(sctx, dc, node).await?;
    let mut out = CappedString::new(MAX_TEXT_BYTES);
    // A `CappedString` never fails.
    let _ignored = writeln!(out, "{}", node.label());
    let _ignored = writeln!(out, "  rule        {}", node.rule_type());
    let _ignored = match location.line {
        Some(line) => writeln!(out, "  build file  {}:{line}", location.shown),
        None => writeln!(out, "  build file  {}", location.shown),
    };
    let fmt = AttrFmtContext {
        package: Some(node.label().pkg().dupe()),
        options: AttrFmtOptions {
            exclude_quotes: false,
        },
    };
    let mut attrs: Vec<_> = node.attrs(AttrInspectOptions::DefinedOnly).collect();
    attrs.sort_by_key(|a| a.name);
    let _ignored = writeln!(out, "  attributes  (those not left to their defaults)");
    let width = attrs
        .iter()
        .map(|a| a.name.len())
        .max()
        .unwrap_or(0)
        .min(32);
    for attr in &attrs {
        let mut value = CappedString::new(MAX_ATTR_BYTES);
        // Stops the formatting once the cap is reached, so a huge value costs no more.
        let _ignored = fmt::write(
            &mut StopWhenFull(&mut value),
            format_args!("{}", attr.value.as_display(&fmt)),
        );
        let ellipsis = if value.truncated() { "…" } else { "" };
        // On one line: strings are shown as they are.
        let value: String = value
            .as_str()
            .chars()
            .flat_map(|c| {
                if c.is_control() {
                    c.escape_default().collect::<Vec<_>>()
                } else {
                    vec![c]
                }
            })
            .collect();
        let _ignored = writeln!(out, "    {:<width$} = {value}{ellipsis}", attr.name);
    }
    let deps: Vec<&TargetLabel> = node.deps().collect();
    let _ignored = write!(out, "  deps        {}", deps.len());
    for (i, dep) in deps.iter().take(MAX_DEPS_SHOWN).enumerate() {
        let sep = if i == 0 { ": " } else { ", " };
        let _ignored = write!(out, "{sep}{dep}");
    }
    if deps.len() > MAX_DEPS_SHOWN {
        let _ignored = write!(out, " and {} more", deps.len() - MAX_DEPS_SHOWN);
    }
    Ok(with_cut_note(out))
}

/// A writer that fails once the `CappedString` under it is full, which ends the formatting.
struct StopWhenFull<'a>(&'a mut CappedString);

impl fmt::Write for StopWhenFull<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.write_str(s)?;
        if self.0.truncated() {
            Err(fmt::Error)
        } else {
            Ok(())
        }
    }
}

/// The pattern `:ls` lists: the session's package by default, and the targets of a package
/// when a package is named (`lib`, `//lib`).
fn ls_pattern(package: &str) -> String {
    let package = package.trim();
    if package.is_empty() {
        ":".to_owned()
    } else if package.ends_with(':') || package.ends_with("...") || {
        // A target (`//lib:a`); the `:` of a cell alias `@cell//` or `cell//` does not count.
        let after_cell = package.find("//").map_or(0, |i| i + 2);
        package.get(after_cell..).is_some_and(|p| p.contains(':'))
    } {
        package.to_owned()
    } else {
        format!("{package}:")
    }
}

/// `:ls [package]`: the targets matched, one `label  rule` line each, sorted.
async fn ls(
    sctx: &dyn ServerCommandContextTrait,
    dc: &mut DiceComputations<'_>,
    package: &str,
) -> Result<String, ReplFailure> {
    let pattern = ls_pattern(package);
    let resolved = parse_and_resolve_patterns_from_cli_args::<TargetPatternExtra>(
        dc,
        &[pattern],
        sctx.working_dir(),
    )
    .await
    .map_err(buck)?;
    // The packages are loaded concurrently, as `buck2 targets` does.
    let packages = dc
        .compute_join(resolved.specs, async |dc, (package, spec)| {
            let results = dc.get_interpreter_results(package.package.dupe()).await?;
            let (targets, missing) = results.apply_spec(spec);
            if let Some(missing) = missing
                && let Some(e) = missing.into_all_errors().next()
            {
                return Err(buck2_error::Error::from(e));
            }
            Ok(targets
                .values()
                .map(|node| (node.label().to_string(), node.rule_type().name().to_owned()))
                .collect::<Vec<_>>())
        })
        .await;
    let mut rows: Vec<(String, String)> = Vec::new();
    for package in packages {
        rows.extend(package.map_err(buck)?);
    }
    rows.sort();
    let width = rows.iter().map(|(l, _)| l.len()).max().unwrap_or(0).min(60);
    let mut out = CappedString::new(MAX_STREAM_BYTES);
    if rows.is_empty() {
        let _ignored = write!(out, "(no targets)");
    }
    for (i, (label, rule)) in rows.iter().enumerate() {
        let newline = if i == 0 { "" } else { "\n" };
        let _ignored = write!(out, "{newline}{label:<width$}  {rule}");
    }
    Ok(with_cut_note(out))
}

/// The text, with a last line that says so if it was cut.
fn with_cut_note(out: CappedString) -> String {
    let truncated = out.truncated();
    let mut out = out.into_string();
    if truncated {
        out.push_str("\n… (the rest was cut)");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ls_pattern() {
        assert_eq!(ls_pattern(""), ":");
        assert_eq!(ls_pattern("lib"), "lib:");
        assert_eq!(ls_pattern("//lib"), "//lib:");
        assert_eq!(ls_pattern("//lib:"), "//lib:");
        assert_eq!(ls_pattern("//lib:a"), "//lib:a");
        assert_eq!(ls_pattern("//lib/..."), "//lib/...");
        assert_eq!(ls_pattern("cell//lib"), "cell//lib:");
        assert_eq!(ls_pattern("@cell//lib"), "@cell//lib:");
        assert!(is_module("//pkg:x.bzl"));
        assert!(!is_module("//pkg:x"));
    }
}
