/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! `:bxl <file.bxl:function> [-- args...]`: runs a BXL function of a file exactly as
//! `buck2 bxl` does (its own `BxlKey`, so it may declare and build actions), in the request's
//! transaction; and the BXL functions of a file, for completion.

use buck2_build_api::materialize::MaterializationAndUploadContext;
use buck2_cli_proto::TargetCfg;
use buck2_cli_proto::repl_candidate;
use buck2_cli_proto::repl_output;
use buck2_common::dice::cells::HasCellResolver;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_interpreter::load_module::InterpreterCalculation;
use buck2_interpreter::paths::module::StarlarkModulePath;
use buck2_repl_syntax::matching::match_tier;
use buck2_server_ctx::ctx::ServerCommandContextTrait;
use dice::DiceComputations;
use dice::DiceTransaction;
use dupe::Dupe;

use crate::bxl::starlark_defs::bxl_function::FrozenBxlFunction;
use crate::command::BxlRun;
use crate::command::BxlRunOutcome;
use crate::command::parse_bxl_label_from_cli;
use crate::command::run_bxl_in_txn;
use crate::repl::complete::candidates::Candidates;
use crate::repl::output::ReplEmitter;
use crate::repl::output::ReplOutputWriter;

/// `:bxl <file.bxl:function> [-- args...]`.
#[derive(Clone, Debug)]
pub(crate) struct BxlSpec {
    /// `<file.bxl>:<function>`, as typed: the file is a label or a path relative to the
    /// session's directory.
    pub(crate) label: String,
    /// The function's command-line arguments.
    pub(crate) args: Vec<String>,
}

/// Runs the BXL function of `spec` as `buck2 bxl` does, with the session's target platform.
/// `ctx.output.stream` output is shown as it is written (by the console), the output of
/// `ctx.output.print` goes to the client's stdout once the artifacts the function ensured are
/// materialized (as `MaterializationAndUploadContext::materialize()`), which this also does.
/// Returns the errors of materializing them.
///
/// Dropping the future stops the run: it is only DICE work.
pub(crate) async fn run_bxl(
    sctx: &dyn ServerCommandContextTrait,
    txn: &DiceTransaction,
    target_cfg: &TargetCfg,
    spec: &BxlSpec,
    emitter: &ReplEmitter,
    id: u64,
) -> buck2_error::Result<Vec<buck2_error::Error>> {
    let mut stdout = ReplOutputWriter::new(emitter.dupe(), id, repl_output::Channel::Stdout);
    let run = BxlRun {
        bxl_label: &spec.label,
        bxl_args: &spec.args,
        target_cfg,
        print_stacktrace: false,
        materialization_context: MaterializationAndUploadContext::materialize(),
    };
    let outcome = run_bxl_in_txn(sctx, txn, run, &mut stdout, |error| {
        emitter.output(id, repl_output::Channel::Stderr, error);
        Ok(())
    })
    .await?;
    Ok(match outcome {
        // The help went to the console.
        BxlRunOutcome::Help => Vec::new(),
        BxlRunOutcome::Ran { errors, .. } => errors,
    })
}

/// The BXL functions of the file `module` (as `:bxl` takes it: a label, or a path relative to
/// `cwd`) whose names match `prefix`, as `<module>:<name>`. Names that start with `_` only if
/// `prefix` does.
pub(crate) async fn complete_bxl_functions(
    dc: &mut DiceComputations<'_>,
    cwd: &ProjectRelativePath,
    module: &str,
    prefix: &str,
) -> buck2_error::Result<Candidates> {
    let mut candidates = Candidates::default();
    // A function name has no `:`, so the label splits back into `module` and `prefix`.
    if prefix.contains(':') {
        return Ok(candidates);
    }
    let cell_resolver = dc.get_cell_resolver().await?;
    let cell_alias_resolver = dc.get_cell_alias_resolver_for_dir(cwd).await?;
    let label = parse_bxl_label_from_cli(
        cwd,
        &format!("{module}:{prefix}"),
        cell_resolver,
        cell_alias_resolver,
    )?;
    let loaded = dc
        .get_loaded_module(StarlarkModulePath::BxlFile(&label.bxl_path))
        .await?;
    let env = loaded.env();
    for name in env.names() {
        if match_tier(prefix, name).is_none() || (name.starts_with('_') && !prefix.starts_with('_'))
        {
            continue;
        }
        let Ok(Some(value)) = env.get_option_ref(name) else {
            continue;
        };
        if value.value().downcast_ref::<FrozenBxlFunction>().is_some() {
            candidates.offer(
                prefix,
                name,
                format!("{module}:{name}"),
                repl_candidate::Kind::Function,
                "",
            );
        }
    }
    Ok(candidates)
}
