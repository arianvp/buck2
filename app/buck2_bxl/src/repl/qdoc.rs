/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! `:qdoc [function|uquery|cquery|aquery]`: the documentation of the functions of the query
//! languages, as `buck2 docs uquery` (and `cquery`, `aquery`) shows it.

use std::collections::BTreeMap;
use std::fmt::Write;

use buck2_cli_proto::repl_error;
use buck2_query::query::syntax::simple::functions::description::QUERY_ENVIRONMENT_DESCRIPTION_BY_TYPE;
use buck2_query::query::syntax::simple::functions::description::QueryType;
use buck2_query::query::syntax::simple::functions::docs::FunctionDescription;
use buck2_query::query::syntax::simple::functions::docs::MarkdownOptions;
use buck2_query::query::syntax::simple::functions::docs::QueryEnvironmentDescription;
use buck2_repl_syntax::matching::Ranked;
use buck2_repl_syntax::matching::match_tier;

use crate::repl::render::ReplFailure;

/// The query languages, as `:qdoc` names them.
const DIALECTS: [(&str, &str); 3] = [("uquery", "u"), ("cquery", "c"), ("aquery", "a")];

/// Most names suggested for an unknown function.
const MAX_SUGGESTIONS: usize = 8;

/// A function, with the languages that have it (by their index in [`DIALECTS`]).
struct Function<'a> {
    description: &'a FunctionDescription,
    dialects: Vec<usize>,
}

/// The text of `:qdoc <arg>`, or why there is none.
pub(crate) fn qdoc(arg: &str) -> Result<String, ReplFailure> {
    let describe = QUERY_ENVIRONMENT_DESCRIPTION_BY_TYPE
        .get()
        .map_err(|e| ReplFailure::from_buck2(repl_error::Kind::Internal, &e))?;
    let descriptions: Vec<QueryEnvironmentDescription> =
        [QueryType::Uquery, QueryType::Cquery, QueryType::Aquery]
            .into_iter()
            .map(describe)
            .collect();
    let mut functions: BTreeMap<&str, Function> = BTreeMap::new();
    for (i, description) in descriptions.iter().enumerate() {
        for module in &description.mods {
            for (name, function) in &module.functions {
                functions
                    .entry(name)
                    .or_insert_with(|| Function {
                        description: function,
                        dialects: Vec::new(),
                    })
                    .dialects
                    .push(i);
            }
        }
    }

    let arg = arg.trim();
    let name = arg.trim_end_matches("()").trim_end_matches('(');
    if arg.is_empty() {
        return Ok(listing(
            "Query functions (u: uquery, c: cquery, a: aquery; `:qdoc <function>` shows one):",
            functions.values(),
        ));
    }
    if let Some(dialect) = DIALECTS.iter().position(|(d, _)| *d == arg) {
        return Ok(listing(
            &format!("The functions of {arg} (`:qdoc <function>` shows one):"),
            functions.values().filter(|f| f.dialects.contains(&dialect)),
        ));
    }
    match functions.get(name) {
        Some(function) => Ok(details(function)),
        None => {
            let mut similar = Ranked::default();
            for candidate in functions.keys() {
                if let Some(tier) = match_tier(name, candidate) {
                    similar.offer(tier, *candidate);
                }
            }
            let similar = similar.into_items();
            let mut message = format!(
                "unknown query function `{name}` (`:qdoc` lists them, `:qdoc cquery` those of \
                 cquery)"
            );
            if !similar.is_empty() {
                message.push_str("; did you mean ");
                for (i, s) in similar.iter().take(MAX_SUGGESTIONS).enumerate() {
                    if i > 0 {
                        message.push_str(", ");
                    }
                    message.push_str(s);
                }
                message.push('?');
            }
            Err(ReplFailure::new(repl_error::Kind::Usage, &message))
        }
    }
}

/// `name(arg, ...)`.
fn signature(function: &FunctionDescription) -> String {
    let args: Vec<&str> = function.args.iter().map(|a| a.name.as_str()).collect();
    format!("{}({})", function.name, args.join(", "))
}

/// The languages that have a function: `u c a`, with `-` for those that do not.
fn dialect_marks(function: &Function) -> String {
    DIALECTS
        .iter()
        .enumerate()
        .map(|(i, (_, mark))| {
            if function.dialects.contains(&i) {
                *mark
            } else {
                "-"
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn listing<'a>(heading: &str, functions: impl Iterator<Item = &'a Function<'a>>) -> String {
    let rows: Vec<(String, String, String)> = functions
        .map(|f| {
            (
                signature(f.description),
                dialect_marks(f),
                f.description
                    .short_help
                    .as_deref()
                    .and_then(|h| h.lines().next())
                    .unwrap_or("")
                    .to_owned(),
            )
        })
        .collect();
    let width = rows.iter().map(|r| r.0.len()).max().unwrap_or(0).min(40);
    let mut out = String::new();
    out.push_str(heading);
    for (signature, marks, help) in rows {
        // Writing to a `String` cannot fail.
        let _ignored = write!(out, "\n  {signature:<width$}  {marks}  {help}");
    }
    out
}

fn details(function: &Function) -> String {
    let mut out = function
        .description
        .render_markdown_for_details(&MarkdownOptions {
            links_enabled: false,
        });
    let dialects: Vec<&str> = function
        .dialects
        .iter()
        .filter_map(|i| DIALECTS.get(*i).map(|(d, _)| *d))
        .collect();
    // Writing to a `String` cannot fail.
    let _ignored = write!(out, "Available in: {}", dialects.join(", "));
    out.trim_start_matches("---\n").to_owned()
}
