/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! The functions of the query languages, for completion: the client completes words of queries
//! with them, and uses the types of their arguments to complete those.

use buck2_cli_proto::ReplCompletions;
use buck2_cli_proto::repl_candidate;
use buck2_cli_proto::repl_complete;
use buck2_cli_proto::repl_completions;
use buck2_query::query::syntax::simple::functions::description::QUERY_ENVIRONMENT_DESCRIPTION_BY_TYPE;
use buck2_query::query::syntax::simple::functions::description::QueryType;
use buck2_query::query::syntax::simple::functions::helpers::QueryArgType;
use buck2_repl_syntax::query::OPERATOR_WORDS;
use buck2_repl_syntax::query::QueryArg;
use buck2_repl_syntax::query::encode_query_args;

use crate::repl::complete::candidates::Candidates;
use crate::repl::complete::candidates::completions_status;

/// Every function of the query language `dialect`, as `name(`, with the types of its arguments
/// in the candidate's detail. The binary operators (`union`, ...) are not functions.
pub(crate) fn query_functions(dialect: repl_complete::QueryDialect) -> ReplCompletions {
    let describe = match QUERY_ENVIRONMENT_DESCRIPTION_BY_TYPE.get() {
        Ok(describe) => describe,
        Err(e) => {
            return completions_status(repl_completions::Status::Error, &format!("{e}"));
        }
    };
    let description = describe(match dialect {
        repl_complete::QueryDialect::Uquery => QueryType::Uquery,
        repl_complete::QueryDialect::Cquery => QueryType::Cquery,
        repl_complete::QueryDialect::Aquery => QueryType::Aquery,
    });
    let mut candidates = Candidates::default();
    for module in &description.mods {
        for (name, function) in &module.functions {
            if OPERATOR_WORDS.contains(name) {
                continue;
            }
            let args = encode_query_args(function.args.iter().map(|arg| query_arg(arg.arg_type)));
            candidates.add(format!("{name}("), repl_candidate::Kind::Function, &args);
        }
    }
    candidates.into_completions()
}

fn query_arg(arg: QueryArgType) -> QueryArg {
    match arg {
        QueryArgType::String => QueryArg::String,
        QueryArgType::Integer => QueryArg::Integer,
        QueryArgType::TargetSet | QueryArgType::Set => QueryArg::Targets,
        QueryArgType::FileSet => QueryArg::Files,
        QueryArgType::Expression | QueryArgType::Value => QueryArg::Expression,
    }
}
