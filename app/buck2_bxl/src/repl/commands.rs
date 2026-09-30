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

use std::borrow::Cow;

use buck2_cli_proto::repl_error;
use buck2_cli_proto::repl_output;
use buck2_repl_syntax::commands::ArgError;
use buck2_repl_syntax::commands::CommandId;
use buck2_repl_syntax::commands::Handler;
use buck2_repl_syntax::commands::ParsedCommand;
use buck2_repl_syntax::commands::QueryDialect;
use buck2_repl_syntax::commands::parse_bxl_args;
use buck2_repl_syntax::commands::parse_load_args;
use buck2_repl_syntax::commands::parse_run_args;
use buck2_repl_syntax::commands::parse_set_args;
use buck2_repl_syntax::commands::split_args;
use buck2_repl_syntax::text::starlark_string_literal;

use crate::repl::build::BuildSpec;
use crate::repl::build::RunSpec;
use crate::repl::bxl::BxlSpec;
use crate::repl::inspect::InspectSpec;
use crate::repl::qdoc::qdoc;
use crate::repl::render::RenderMode;
use crate::repl::render::ReplFailure;
use crate::repl::settings::SetWork;
use crate::repl::settings::set_work;
use crate::repl::thread::EvalKind;

/// Deepest nesting of parentheses in the query of `:uquery`, `:cquery` and `:aquery`. buck2's
/// query parser recurses on it: a few thousand levels run out of native stack, which aborts the
/// daemon (as `buck2 cquery` does with them).
const MAX_QUERY_DEPTH: usize = 500;

/// Most `/`-separated parts of a target pattern given to a command. buck2 resolves a pattern
/// recursively on its parts: a few thousand run out of native stack, which aborts the daemon (as
/// `buck2 targets` does with them).
const MAX_PATTERN_PARTS: usize = 1000;

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
    /// `:info`, `:ls`, `:__locate`: the target graph, read in a transaction.
    Inspect(InspectSpec),
    /// `:set`, for the settings of the daemon.
    Set(SetWork),
    /// Text made at once, without DICE (`:qdoc`), and how the client shows it.
    Text(String, repl_output::Format),
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
        CommandId::Reload => eval(EvalKind::Reload { edited: None }, String::new()),
        CommandId::Reset => Ok(CommandWork::Reset),
        CommandId::Uquery => eval(
            EvalKind::Sugar,
            query_code(QueryDialect::Uquery, &query(arg)?),
        ),
        CommandId::Cquery => eval(
            EvalKind::Sugar,
            query_code(QueryDialect::Cquery, &query(arg)?),
        ),
        CommandId::Aquery => eval(
            EvalKind::Sugar,
            query_code(QueryDialect::Aquery, &query(arg)?),
        ),
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
            check_pattern(&target)?;
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
            patterns.iter().try_for_each(|p| check_pattern(p))?;
            Ok(CommandWork::Build(BuildSpec {
                patterns,
                run: None,
            }))
        }
        CommandId::Run => {
            let args = parse_run_args(arg).map_err(|e| usage_error(command, &e))?;
            check_pattern(&args.target)?;
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
                // As for `:load`, a leading `./` (which completion offers) is dropped.
                label: load_module(&args.label).to_owned(),
                args: args.args,
            }))
        }
        CommandId::Info => Ok(CommandWork::Inspect(InspectSpec::Info {
            target: check_pattern_word(one_word(command)?)?,
        })),
        CommandId::Ls => {
            let words = split_args(arg).map_err(|e| usage_error(command, &e))?;
            let package = match <[String; 1]>::try_from(words) {
                Ok([package]) => package,
                Err(words) if words.is_empty() => String::new(),
                Err(_) => return Err(one_argument(command)),
            };
            check_pattern(&package)?;
            Ok(CommandWork::Inspect(InspectSpec::Ls { package }))
        }
        CommandId::Locate => Ok(CommandWork::Inspect(InspectSpec::Locate {
            what: check_pattern_word(one_word(command)?)?,
        })),
        CommandId::Edited => eval(
            EvalKind::Reload {
                edited: Some(one_word(command)?),
            },
            String::new(),
        ),
        CommandId::Who => {
            let globs = split_args(arg).map_err(|e| usage_error(command, &e))?;
            eval(EvalKind::Who { globs }, String::new())
        }
        CommandId::Set => {
            let args = parse_set_args(arg).map_err(|e| usage_error(command, &e))?;
            Ok(CommandWork::Set(set_work(args)?))
        }
        CommandId::Qdoc => {
            let (text, format) = qdoc(arg)?;
            Ok(CommandWork::Text(text, format))
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

/// The query of a query command: its argument, unless it is one word quoted as on the shell
/// (`:cquery 'deps(//x)'`), which is unquoted (the command takes the query as it is typed: the
/// quoted text would be one target literal). Fails if its parentheses nest too deeply.
fn query(arg: &str) -> Result<Cow<'_, str>, ReplFailure> {
    let trimmed = arg.trim();
    let quote = trimmed.chars().next().filter(|c| matches!(c, '\'' | '"'));
    let unquoted = match quote {
        Some(quote) if trimmed.len() >= 2 && trimmed.ends_with(quote) => split_args(trimmed)
            .ok()
            .and_then(|words| <[String; 1]>::try_from(words).ok())
            .map(|[word]| word),
        _ => None,
    };
    let query = unquoted.map_or(Cow::Borrowed(arg), Cow::Owned);
    let mut depth = 0usize;
    for c in query.chars() {
        match c {
            '(' => {
                depth += 1;
                if depth > MAX_QUERY_DEPTH {
                    return Err(ReplFailure::new(
                        repl_error::Kind::Usage,
                        &format_args!(
                            "the query nests more than {MAX_QUERY_DEPTH} levels of parentheses"
                        ),
                    ));
                }
            }
            ')' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(query)
}

/// Fails if the target pattern has too many parts to resolve safely.
fn check_pattern(pattern: &str) -> Result<(), ReplFailure> {
    if pattern.split('/').count() > MAX_PATTERN_PARTS {
        return Err(ReplFailure::new(
            repl_error::Kind::Usage,
            &format_args!("the pattern has more than {MAX_PATTERN_PARTS} `/`-separated parts"),
        ));
    }
    Ok(())
}

fn check_pattern_word(word: String) -> Result<String, ReplFailure> {
    check_pattern(&word)?;
    Ok(word)
}

/// The one word of the argument (shell-split).
fn one_word(command: &ParsedCommand<'_>) -> Result<String, ReplFailure> {
    let words = split_args(&command.arg).map_err(|e| usage_error(command, &e))?;
    match <[String; 1]>::try_from(words) {
        Ok([word]) => Ok(word),
        Err(_) => Err(one_argument(command)),
    }
}

fn one_argument(command: &ParsedCommand<'_>) -> ReplFailure {
    ReplFailure::new(
        repl_error::Kind::Usage,
        &format_args!(
            "`{}` takes one argument; usage: {}",
            command.spec.display_name(),
            command.spec.usage
        ),
    )
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
    fn test_query() {
        assert_eq!(query("deps(//a)").ok().as_deref(), Some("deps(//a)"));
        assert_eq!(query("'deps(//a)'").ok().as_deref(), Some("deps(//a)"));
        assert_eq!(query(" \"deps(//a)\" ").ok().as_deref(), Some("deps(//a)"));
        assert_eq!(query("'a' + 'b'").ok().as_deref(), Some("'a' + 'b'"));
        assert_eq!(query("'").ok().as_deref(), Some("'"));
        let deep = format!("{}//a{}", "deps(".repeat(500), ")".repeat(500));
        assert!(query(&deep).is_ok());
        let deeper = format!("{}//a{}", "deps(".repeat(501), ")".repeat(501));
        assert!(query(&deeper).is_err());
        assert!(check_pattern(&"a/".repeat(999)).is_ok());
        assert!(check_pattern(&"a/".repeat(1000)).is_err());
    }

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
