/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Completion of the symbols of a module to load (`load("//pkg:x.bzl", "dou`), from DICE.

use buck2_core::bxl::BxlFilePath;
use buck2_core::cells::cell_path::CellPath;
use buck2_fs::paths::forward_rel_path::ForwardRelativePath;
use buck2_interpreter::load_module::InterpreterCalculation;
use buck2_interpreter::paths::path::OwnedStarlarkPath;
use buck2_interpreter::paths::path::StarlarkPath;
use buck2_interpreter_for_build::interpreter::dice_calculation_delegate::HasCalculationDelegate;
use buck2_repl_syntax::matching::match_tier;
use dice::DiceComputations;

use crate::repl::complete::candidates::Candidates;
use crate::repl::complete::names::kind_of;
use crate::repl::session::REPL_FILE_NAME;

/// The public symbols of `module` (loaded as `load()` at the prompt loads it: relative to the
/// session's directory `cwd`) that match `prefix`, but those in `used`. Symbols whose names start
/// with `_` cannot be loaded.
pub(crate) async fn complete_load_symbols(
    dc: &mut DiceComputations<'_>,
    cwd: &CellPath,
    module: &str,
    prefix: &str,
    used: &[String],
) -> buck2_error::Result<Candidates> {
    let mut candidates = Candidates::default();
    // As `:load` takes it.
    let mut module = module;
    while let Some(rest) = module.strip_prefix("./") {
        module = rest;
    }
    let repl_path = BxlFilePath::new(cwd.join(ForwardRelativePath::new(REPL_FILE_NAME)?))?;
    let path = {
        let calc = dc
            .get_interpreter_calculator(OwnedStarlarkPath::BxlFile(repl_path.clone()))
            .await?;
        calc.resolve_load(StarlarkPath::BxlFile(&repl_path), module)
            .await?
    };
    let loaded = dc.get_loaded_module(path.borrow()).await?;
    let env = loaded.env();
    let mut names: Vec<&str> = env
        .names()
        .filter(|name| {
            !name.starts_with('_')
                && match_tier(prefix, name).is_some()
                && !used.iter().any(|u| u == name)
        })
        .collect();
    names.sort_unstable();
    for name in names {
        if candidates.is_full() {
            candidates.mark_truncated();
            break;
        }
        let Ok(Some(value)) = env.get_option_ref(name) else {
            continue;
        };
        let value = value.value();
        candidates.offer(
            prefix,
            name,
            name.to_owned(),
            kind_of(value),
            value.get_type(),
        );
    }
    Ok(candidates)
}
