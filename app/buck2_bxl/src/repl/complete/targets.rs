/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Completion of target patterns and of the modules to load, from DICE: cells, directories,
//! packages, target names, subtargets, and `.bzl`/`.bxl` files.
//!
//! A pattern without a cell (`pkg/sub`, `:name`) is relative to the session's directory, as
//! everything is in the session (spec §0.5).

use buck2_build_api::analysis::calculation::RuleAnalysisCalculation;
use buck2_cli_proto::TargetCfg;
use buck2_cli_proto::repl_candidate;
use buck2_common::dice::cells::HasCellResolver;
use buck2_common::file_ops::dice::DiceFileComputations;
use buck2_common::file_ops::metadata::FileType;
use buck2_common::file_ops::metadata::ReadDirOutput;
use buck2_common::file_ops::metadata::SimpleDirEntry;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::paths::CellRelativePath;
use buck2_core::package::PackageLabel;
use buck2_fs::paths::file_name::FileNameBuf;
use buck2_fs::paths::forward_rel_path::ForwardRelativePath;
use buck2_node::nodes::eval_result::is_generated_target;
use buck2_node::nodes::frontend::TargetGraphCalculation;
use buck2_repl_syntax::matching::Ranked;
use buck2_repl_syntax::matching::match_tier;
use buck2_server_ctx::ctx::ServerCommandContextTrait;
use buck2_server_ctx::pattern_parse_and_resolve::parse_and_resolve_provider_labels_with_modifiers_from_cli_args;
use buck2_server_ctx::target_resolution_config::TargetResolutionConfig;
use dice::DiceComputations;

use crate::repl::complete::candidates::Candidates;

/// Most subdirectories of a directory that are checked for a build file (to offer `dir:` as
/// well as `dir/`), or for modules to load.
const MAX_PACKAGE_CHECKS: usize = 32;

/// The extensions of the files `load` loads.
const LOAD_EXTENSIONS: &[&str] = &[".bzl", ".bxl"];

/// What a path completes to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Listing {
    /// Target patterns: `dir/`, `pkg:`, `pkg:target`, `dir/...`.
    Targets,
    /// Modules to load: `@cell//`, `dir/`, `dir:`, `dir:file.bzl`. After the colon comes a file
    /// name, never a path (`//pkg:sub/x.bzl` is rejected by buck2), and a label that is not
    /// relative to the working directory names its cell (`//pkg:x.bzl`, not `pkg:x.bzl`, which
    /// buck2 rejects too). `load()` takes a cell only with an `@` (`@cell//x:y.bzl`; `:bxl`
    /// takes both).
    LoadFiles,
}

/// The candidates for the target pattern `prefix`, relative to `cwd`: `//x:y[` completes the
/// subtargets of `//x:y`, configured for `target_cfg`. Errors (an unknown package, a directory
/// that cannot be read, a target that fails analysis) are errors of the request.
pub(crate) async fn complete_targets(
    dc: &mut DiceComputations<'_>,
    sctx: &dyn ServerCommandContextTrait,
    target_cfg: &TargetCfg,
    cwd: &CellPath,
    prefix: &str,
) -> buck2_error::Result<Candidates> {
    if prefix.contains('[') {
        return complete_subtargets(dc, sctx, target_cfg, prefix).await;
    }
    complete_paths(dc, cwd, prefix, Listing::Targets).await
}

/// The candidates for the module to load `prefix` (`//pkg:x.b`), relative to `cwd`.
pub(crate) async fn complete_load_paths(
    dc: &mut DiceComputations<'_>,
    cwd: &CellPath,
    prefix: &str,
) -> buck2_error::Result<Candidates> {
    complete_paths(dc, cwd, prefix, Listing::LoadFiles).await
}

async fn complete_paths(
    dc: &mut DiceComputations<'_>,
    cwd: &CellPath,
    prefix: &str,
    listing_kind: Listing,
) -> buck2_error::Result<Candidates> {
    let mut candidates = Candidates::default();
    // Modifiers are not completed.
    if prefix.contains(['[', '?', '(']) || prefix.contains(char::is_whitespace) {
        return Ok(candidates);
    }

    // The cell: `cell//rest` or `//rest`, otherwise the session's directory.
    let (typed_cell, base, rest) = match prefix.find("//") {
        Some(i) => {
            let alias = prefix.get(..i).unwrap_or("");
            let alias = alias.strip_prefix('@').unwrap_or(alias);
            let resolver = dc.get_cell_alias_resolver(cwd.cell()).await?;
            let Ok(cell) = resolver.resolve(alias) else {
                // Not a cell: nothing starts with it.
                return Ok(candidates);
            };
            (
                prefix.get(..i + 2).unwrap_or(""),
                CellPath::new(cell, CellRelativePath::empty().to_buf()),
                prefix.get(i + 2..).unwrap_or(""),
            )
        }
        None => ("", cwd.clone(), prefix),
    };

    if listing_kind == Listing::LoadFiles && typed_cell.is_empty() && !prefix.starts_with(':') {
        // Not a label buck2 loads (`pkg:x.bzl`, `@ro`): only the start of a cell (the client
        // completes paths relative to the working directory).
        if !prefix.contains(['/', ':']) {
            let resolver = dc.get_cell_alias_resolver(cwd.cell()).await?;
            let typed = prefix.strip_prefix('@').unwrap_or(prefix);
            for (alias, _) in resolver.mappings() {
                let cell = format!("{}//", alias.as_str());
                candidates.offer(
                    typed,
                    &cell,
                    format!("@{cell}"),
                    repl_candidate::Kind::Cell,
                    "",
                );
            }
        }
        return Ok(candidates);
    }

    if let Some((package, name_prefix)) = rest.split_once(':') {
        let typed_package = prefix.get(..prefix.len() - name_prefix.len()).unwrap_or("");
        let Some(dir) = join(&base, package) else {
            return Ok(candidates);
        };
        match listing_kind {
            Listing::Targets => {
                // Targets of a package.
                let package_label = PackageLabel::from_cell_path(dir.as_ref())?;
                let results = dc.get_interpreter_results(package_label).await?;
                let mut names: Vec<&str> = results
                    .targets()
                    .iter()
                    .filter(|(_, node)| !is_generated_target(*node))
                    .map(|(name, _)| name.as_str())
                    .collect();
                names.sort_unstable();
                for name in names {
                    if candidates.is_full() {
                        candidates.mark_truncated();
                        break;
                    }
                    candidates.offer(
                        name_prefix,
                        name,
                        format!("{typed_package}{name}"),
                        repl_candidate::Kind::Target,
                        "",
                    );
                }
            }
            Listing::LoadFiles => {
                // The modules of the directory (a file name, not a path, follows the colon).
                if name_prefix.contains('/') {
                    return Ok(candidates);
                }
                let listing = DiceFileComputations::read_dir(dc, dir.as_ref()).await?;
                for entry in listing.included.iter() {
                    let name = entry.file_name.as_str();
                    if (name.starts_with('.') && !name_prefix.starts_with('.'))
                        || !is_load_file(entry)
                    {
                        continue;
                    }
                    candidates.offer(
                        name_prefix,
                        name,
                        format!("{typed_package}{name}"),
                        repl_candidate::Kind::File,
                        "",
                    );
                }
            }
        }
        return Ok(candidates);
    }

    // Directories: `dir_part` is what is typed of the directory, `fragment` the start of the name
    // of one of its subdirectories.
    let (dir_part, fragment) = split_last_slash(rest);
    let Some(dir) = join(&base, dir_part) else {
        return Ok(candidates);
    };
    let typed_dir = prefix.get(..prefix.len() - fragment.len()).unwrap_or("");

    if typed_cell.is_empty() && !rest.contains('/') {
        // The start of a cell name.
        let resolver = dc.get_cell_alias_resolver(cwd.cell()).await?;
        for (alias, _) in resolver.mappings() {
            let cell = format!("{}//", alias.as_str());
            candidates.offer(prefix, &cell, cell.clone(), repl_candidate::Kind::Cell, "");
        }
    }

    let listing = DiceFileComputations::read_dir(dc, dir.as_ref()).await?;
    let buildfiles = DiceFileComputations::buildfiles(dc, dir.cell()).await?;
    let cell_resolver = dc.get_cell_resolver().await?;
    // The subdirectories that match best (the others would be dropped).
    let mut subdirs = Ranked::default();
    for entry in listing.included.iter() {
        let name = entry.file_name.as_str();
        if entry.file_type != FileType::Directory
            || (name.starts_with('.') && !fragment.starts_with('.'))
        {
            continue;
        }
        if let Some(tier) = match_tier(fragment, name) {
            subdirs.offer(tier, &entry.file_name);
        }
    }
    let mut checks = 0;
    for name in subdirs.into_items() {
        let subdir = dir.join(name);
        // The root of another cell is not a directory of this one.
        match cell_resolver.resolve_path(subdir.as_ref()) {
            Ok(path) if cell_resolver.find(&path) == dir.cell() => {}
            _ => continue,
        }
        let name = name.as_str();
        candidates.offer(
            fragment,
            name,
            format!("{typed_dir}{name}/"),
            repl_candidate::Kind::Directory,
            "",
        );
        if checks < MAX_PACKAGE_CHECKS {
            checks += 1;
            if let Ok(sub_listing) = DiceFileComputations::read_dir(dc, subdir.as_ref()).await
                && has_marker(listing_kind, sub_listing, buildfiles)
            {
                candidates.offer(
                    fragment,
                    name,
                    format!("{typed_dir}{name}:"),
                    repl_candidate::Kind::Package,
                    "",
                );
            }
        }
    }
    if fragment.is_empty() {
        // The directory itself: its targets or modules, and (for targets) everything below it.
        if has_marker(listing_kind, listing, buildfiles) {
            // `//pkg/` is typed; `//pkg:` does not start with it, so it is only offered for `//`
            // or an empty prefix.
            let package = if typed_dir.ends_with("//") {
                typed_dir
            } else {
                typed_dir.strip_suffix('/').unwrap_or(typed_dir)
            };
            let package = format!("{package}:");
            if package.starts_with(prefix) {
                candidates.offer("", "", package, repl_candidate::Kind::Package, "");
            }
        }
        if listing_kind == Listing::Targets {
            candidates.offer(
                "",
                "",
                format!("{typed_dir}..."),
                repl_candidate::Kind::Pattern,
                "",
            );
        }
    }
    Ok(candidates)
}

/// The subtargets of the target before the last `[` of `prefix` (`//x:y[`, `//x:y[a][`), from
/// its `DefaultInfo`, as `//x:y[name]`.
async fn complete_subtargets(
    dc: &mut DiceComputations<'_>,
    sctx: &dyn ServerCommandContextTrait,
    target_cfg: &TargetCfg,
    prefix: &str,
) -> buck2_error::Result<Candidates> {
    let mut candidates = Candidates::default();
    let Some((base, name_prefix)) = prefix.rsplit_once('[') else {
        return Ok(candidates);
    };
    // `//x:y[a][` names the subtarget `a`: every `[` before the last one is closed. The base
    // names one target, not a package (`//x:`) or a recursive pattern.
    let target = base.split('[').next().unwrap_or(base);
    if name_prefix.contains(']')
        || base.contains(char::is_whitespace)
        || base.matches('[').count() != base.matches(']').count()
        || !target.contains(':')
        || target.ends_with(':')
        || target.ends_with("...")
    {
        return Ok(candidates);
    }
    let labels = parse_and_resolve_provider_labels_with_modifiers_from_cli_args(
        dc,
        &[base.to_owned()],
        sctx.working_dir(),
    )
    .await?;
    let [label] = labels.as_slice() else {
        return Ok(candidates);
    };
    let config = TargetResolutionConfig::from_args(dc, target_cfg, sctx, &[]).await?;
    let configured = config
        .get_configured_provider_label_with_modifiers(dc, label)
        .await?;
    let Some(configured) = configured.first() else {
        return Ok(candidates);
    };
    let providers = dc.get_providers(configured).await?.require_compatible()?;
    let collection = providers.provider_collection();
    let default_info = collection.default_info()?;
    for name in default_info.sub_targets().keys() {
        if candidates.is_full() {
            candidates.mark_truncated();
            break;
        }
        let name: &str = name;
        candidates.offer(
            name_prefix,
            name,
            format!("{base}[{name}]"),
            repl_candidate::Kind::Target,
            "",
        );
    }
    Ok(candidates)
}

/// Splits a typed path at its last `/`: the directory part (with the slash) and the rest.
fn split_last_slash(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (
            path.get(..i + 1).unwrap_or(""),
            path.get(i + 1..).unwrap_or(""),
        ),
        None => ("", path),
    }
}

/// `base` joined with the typed relative path `path` (a trailing slash ignored). `None` if it
/// is not a forward relative path (`..`, `a//b`, `/a`, ...).
fn join(base: &CellPath, path: &str) -> Option<CellPath> {
    // One trailing slash is fine (`pkg/`), `pkg//` and `/pkg` are not paths.
    if path.starts_with('/') || path.ends_with("//") {
        return None;
    }
    let path = ForwardRelativePath::new_trim_trailing_slashes(path).ok()?;
    Some(base.join(path))
}

/// Whether a directory entry is a module `load` loads.
fn is_load_file(entry: &SimpleDirEntry) -> bool {
    entry.file_type != FileType::Directory
        && LOAD_EXTENSIONS
            .iter()
            .any(|e| entry.file_name.as_str().ends_with(e))
}

/// Whether a directory with this listing is a package (targets), or has modules to load.
fn has_marker(listing_kind: Listing, listing: &ReadDirOutput, buildfiles: &[FileNameBuf]) -> bool {
    match listing_kind {
        Listing::Targets => buildfiles.iter().any(|name| listing.contains(name)),
        Listing::LoadFiles => listing.included.iter().any(is_load_file),
    }
}
