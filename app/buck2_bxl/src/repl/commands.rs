/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! The meta-commands the daemon handles: what each one asks of the session.
//!
//! Some are sugar: they are evaluated as Starlark code generated from their argument, whose
//! strings are always written as literals (`:cquery deps(:lib)` is
//! `ctx.cquery().eval("deps(:lib)")`).

use buck2_cli_proto::repl_error;
use buck2_repl_syntax::commands::ArgError;
use buck2_repl_syntax::commands::CommandId;
use buck2_repl_syntax::commands::Handler;
use buck2_repl_syntax::commands::ParsedCommand;
use buck2_repl_syntax::commands::QueryDialect;
use buck2_repl_syntax::commands::parse_bxl_args;
use buck2_repl_syntax::commands::parse_load_args;
use buck2_repl_syntax::commands::parse_run_args;
use buck2_repl_syntax::commands::split_args;
use buck2_repl_syntax::text::starlark_string_literal;

use crate::repl::build::BuildSpec;
use crate::repl::build::RunSpec;
use crate::repl::bxl::BxlSpec;
use crate::repl::render::RenderMode;
use crate::repl::render::ReplFailure;
use crate::repl::thread::EvalKind;

/// What a meta-command asks of the session.
pub(crate) enum CommandWork {
    /// Evaluate `code` (empty for some kinds) in a job of `kind`.
    Eval { kind: EvalKind, code: String },
    /// `:reset`: start a new session.
    Reset,
    /// `:build`, `:run`: a native build.
    Build(BuildSpec),
    /// `:bxl`: a BXL function of a file.
    Bxl(BxlSpec),
}

/// The work of a meta-command, or why it cannot be done.
pub(crate) fn command_work(command: &ParsedCommand<'_>) -> Result<CommandWork, ReplFailure> {
    let arg: &str = &command.arg;
    let eval = |kind, code| Ok(CommandWork::Eval { kind, code });
    match command.spec.id {
        CommandId::Type => eval(EvalKind::Render(RenderMode::Type), arg.to_owned()),
        CommandId::Print => eval(EvalKind::Render(RenderMode::Print), arg.to_owned()),
        CommandId::Json => eval(EvalKind::Render(RenderMode::Json), arg.to_owned()),
        CommandId::Doc => eval(EvalKind::Render(RenderMode::Doc), arg.to_owned()),
        CommandId::Load => {
            let args = parse_load_args(arg).map_err(|e| usage_error(command, &e))?;
            let module = load_module(&args.module).to_owned();
            if args.symbols.is_empty() {
                eval(EvalKind::ImportAll { module }, String::new())
            } else {
                eval(EvalKind::Sugar, load_code(&module, &args.symbols))
            }
        }
        CommandId::Reload => eval(EvalKind::Reload, String::new()),
        CommandId::Reset => Ok(CommandWork::Reset),
        CommandId::Uquery => eval(EvalKind::Sugar, query_code(QueryDialect::Uquery, arg)),
        CommandId::Cquery => eval(EvalKind::Sugar, query_code(QueryDialect::Cquery, arg)),
        CommandId::Aquery => eval(EvalKind::Sugar, query_code(QueryDialect::Aquery, arg)),
        CommandId::Providers => {
            let target = match split_args(arg) {
                Ok(words) => match <[String; 1]>::try_from(words) {
                    Ok([target]) => target,
                    Err(_) => {
                        return Err(ReplFailure::new(
                            repl_error::Kind::Usage,
                            &format_args!(
                                "`:providers` takes one target; usage: {}",
                                command.spec.usage
                            ),
                        ));
                    }
                },
                Err(e) => return Err(usage_error(command, &e)),
            };
            eval(EvalKind::Sugar, providers_code(&target))
        }
        CommandId::Build => {
            let patterns = split_args(arg).map_err(|e| usage_error(command, &e))?;
            if let Some(flag) = patterns.iter().find(|p| p.starts_with('-')) {
                return Err(usage_error(command, &ArgError::UnknownFlag(flag.clone())));
            }
            if patterns.is_empty() {
                return Err(usage_error(command, &ArgError::MissingTarget));
            }
            Ok(CommandWork::Build(BuildSpec {
                patterns,
                run: None,
            }))
        }
        CommandId::Run => {
            let args = parse_run_args(arg).map_err(|e| usage_error(command, &e))?;
            Ok(CommandWork::Build(BuildSpec {
                patterns: vec![args.target],
                run: Some(RunSpec {
                    args: args.args,
                    print: args.print,
                }),
            }))
        }
        CommandId::Bxl => {
            let args = parse_bxl_args(arg).map_err(|e| usage_error(command, &e))?;
            Ok(CommandWork::Bxl(BxlSpec {
                label: args.label,
                args: args.args,
            }))
        }
        _ => {
            let name = command.spec.display_name();
            Err(match command.spec.handler {
                Handler::Client => ReplFailure::new(
                    repl_error::Kind::Usage,
                    &format_args!("`{name}` is handled by the client"),
                ),
                Handler::Server | Handler::Both => ReplFailure::new(
                    repl_error::Kind::Unsupported,
                    &format_args!("`{name}` is not implemented yet"),
                ),
            })
        }
    }
}

fn usage_error(command: &ParsedCommand<'_>, e: &ArgError) -> ReplFailure {
    ReplFailure::new(
        repl_error::Kind::Usage,
        &format_args!("{e}; usage: {}", command.spec.usage),
    )
}

/// The module of `:load` as `load()` takes it. Paths are relative to the session's directory,
/// as `load()` at the prompt resolves them (`x.bzl`, `sub/x.bxl`, `:x.bzl`); a leading `./`,
/// which `load()` rejects, is dropped.
fn load_module(module: &str) -> &str {
    let mut module = module;
    while let Some(rest) = module.strip_prefix("./") {
        module = rest.trim_start_matches('/');
    }
    module
}

/// `load("<module>", "a", "b")`.
fn load_code(module: &str, symbols: &[String]) -> String {
    let mut code = format!("load({}", starlark_string_literal(module));
    for symbol in symbols {
        code.push_str(", ");
        code.push_str(&starlark_string_literal(symbol));
    }
    code.push(')');
    code
}

/// `ctx.cquery().eval("<query>")`: literals in the query are relative to the session's
/// directory.
fn query_code(dialect: QueryDialect, query: &str) -> String {
    format!(
        "ctx.{}().eval({})",
        dialect.ctx_method(),
        starlark_string_literal(query)
    )
}

/// The providers of a target, or of each target of a pattern (`//pkg:`, `//pkg/...`), by label.
///
/// `ctx.analysis` takes configured targets, and returns a dict when given a set of them.
fn providers_code(target: &str) -> String {
    let targets = format!(
        "ctx.configured_targets({})",
        starlark_string_literal(target)
    );
    let is_pattern = target.ends_with(':') || target.ends_with("...");
    if is_pattern {
        format!(
            "{{label: result.providers() for label, result in ctx.analysis({targets}).items()}}"
        )
    } else {
        format!("ctx.analysis({targets}).providers()")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sugar() {
        assert_eq!(
            query_code(QueryDialect::Cquery, "deps(\"//a\")"),
            r#"ctx.cquery().eval("deps(\"//a\")")"#
        );
        assert_eq!(
            load_code(":x.bzl", &["a".to_owned(), "b".to_owned()]),
            r#"load(":x.bzl", "a", "b")"#
        );
        assert_eq!(load_module("./sub/x.bzl"), "sub/x.bzl");
        assert_eq!(
            providers_code("//:hello"),
            r#"ctx.analysis(ctx.configured_targets("//:hello")).providers()"#
        );
        assert!(providers_code("//pkg:").starts_with("{label: "));
        assert!(providers_code("//...").starts_with("{label: "));
    }
}
