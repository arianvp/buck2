/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Completion of target patterns, from DICE: cells, directories, packages and target names.
//!
//! A pattern without a cell (`pkg/sub`, `:name`) is relative to the session's directory, as
//! everything is in the session (spec §0.5).

use buck2_cli_proto::repl_candidate;
use buck2_common::dice::cells::HasCellResolver;
use buck2_common::file_ops::dice::DiceFileComputations;
use buck2_common::file_ops::metadata::FileType;
use buck2_common::file_ops::metadata::ReadDirOutput;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::paths::CellRelativePath;
use buck2_core::package::PackageLabel;
use buck2_fs::paths::file_name::FileNameBuf;
use buck2_fs::paths::forward_rel_path::ForwardRelativePath;
use buck2_node::nodes::eval_result::is_generated_target;
use buck2_node::nodes::frontend::TargetGraphCalculation;
use dice::DiceComputations;

use crate::repl::complete::candidates::Candidates;

/// Most subdirectories of a directory that are checked for a build file (to offer `dir:` as
/// well as `dir/`).
const MAX_PACKAGE_CHECKS: usize = 32;

/// The candidates for the target pattern `prefix`, relative to `cwd`. Errors (an unknown
/// package, a directory that cannot be read) are errors of the request.
pub(crate) async fn complete_targets(
    dc: &mut DiceComputations<'_>,
    cwd: &CellPath,
    prefix: &str,
) -> buck2_error::Result<Candidates> {
    let mut candidates = Candidates::default();
    // Subtargets and modifiers are not completed.
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

    if let Some((package, name_prefix)) = rest.split_once(':') {
        // Targets of a package.
        let Some(dir) = join(&base, package) else {
            return Ok(candidates);
        };
        let package_label = PackageLabel::from_cell_path(dir.as_ref())?;
        let results = dc.get_interpreter_results(package_label).await?;
        let typed_package = prefix.get(..prefix.len() - name_prefix.len()).unwrap_or("");
        let mut names: Vec<&str> = results
            .targets()
            .iter()
            .filter(|(name, node)| {
                name.as_str().starts_with(name_prefix) && !is_generated_target(*node)
            })
            .map(|(name, _)| name.as_str())
            .collect();
        names.sort_unstable();
        for name in names {
            if candidates.is_full() {
                candidates.mark_truncated();
                break;
            }
            candidates.add(
                format!("{typed_package}{name}"),
                repl_candidate::Kind::Target,
                "",
            );
        }
        return Ok(candidates);
    }

    // Directories: `dir_part` is what is typed of the directory, `fragment` the start of the name
    // of one of its subdirectories.
    let (dir_part, fragment) = match rest.rfind('/') {
        Some(i) => (
            rest.get(..i + 1).unwrap_or(""),
            rest.get(i + 1..).unwrap_or(""),
        ),
        None => ("", rest),
    };
    let Some(dir) = join(&base, dir_part) else {
        return Ok(candidates);
    };
    let typed_dir = prefix.get(..prefix.len() - fragment.len()).unwrap_or("");

    if typed_cell.is_empty() && !rest.contains('/') {
        // The start of a cell name.
        let resolver = dc.get_cell_alias_resolver(cwd.cell()).await?;
        for (alias, _) in resolver.mappings() {
            let cell = format!("{}//", alias.as_str());
            if cell.starts_with(prefix) {
                candidates.add(cell, repl_candidate::Kind::Cell, "");
            }
        }
    }

    let listing = DiceFileComputations::read_dir(dc, dir.as_ref()).await?;
    let buildfiles = DiceFileComputations::buildfiles(dc, dir.cell()).await?;
    let cell_resolver = dc.get_cell_resolver().await?;
    let mut checks = 0;
    for entry in listing.included.iter() {
        let name = entry.file_name.as_str();
        if entry.file_type != FileType::Directory
            || !name.starts_with(fragment)
            || (name.starts_with('.') && !fragment.starts_with('.'))
        {
            continue;
        }
        let subdir = dir.join(&entry.file_name);
        // The root of another cell is not a directory of this one.
        match cell_resolver.resolve_path(subdir.as_ref()) {
            Ok(path) if cell_resolver.find(&path) == dir.cell() => {}
            _ => continue,
        }
        candidates.add(
            format!("{typed_dir}{name}/"),
            repl_candidate::Kind::Directory,
            "",
        );
        if checks < MAX_PACKAGE_CHECKS {
            checks += 1;
            if let Ok(sub_listing) = DiceFileComputations::read_dir(dc, subdir.as_ref()).await
                && has_buildfile(sub_listing, buildfiles)
            {
                candidates.add(
                    format!("{typed_dir}{name}:"),
                    repl_candidate::Kind::Package,
                    "",
                );
            }
        }
    }
    if fragment.is_empty() {
        // The directory itself: its targets (if it is a package), and everything below it.
        if has_buildfile(listing, buildfiles) {
            // `//pkg/` is typed; `//pkg:` does not start with it, so it is only offered for `//`
            // or an empty prefix.
            let package = typed_dir.strip_suffix('/').unwrap_or(typed_dir);
            let package = if typed_dir.ends_with("//") {
                typed_dir
            } else {
                package
            };
            candidates.add(format!("{package}:"), repl_candidate::Kind::Package, "");
        }
        candidates.add(format!("{typed_dir}..."), repl_candidate::Kind::Pattern, "");
    }
    candidates.retain_prefix(prefix);
    Ok(candidates)
}

/// `base` joined with the typed relative path `path` (trailing slashes ignored). `None` if it is
/// not a forward relative path (`..`, `a//b`, ...).
fn join(base: &CellPath, path: &str) -> Option<CellPath> {
    let path = ForwardRelativePath::new_trim_trailing_slashes(path).ok()?;
    Some(base.join(path))
}

/// Whether a directory with this listing is a package.
fn has_buildfile(listing: &ReadDirOutput, buildfiles: &[FileNameBuf]) -> bool {
    buildfiles.iter().any(|name| listing.contains(name))
}
