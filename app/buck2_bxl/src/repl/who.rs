/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! `:who [glob...]`: the bindings of the session, with their types and the start of their
//! values. Not the symbols of the prelude, `ctx` or `_`, which every session has.

use std::fmt::Write;

use buck2_interpreter::factory::BuckStarlarkModule;
use buck2_repl_syntax::text::CappedString;
use buck2_repl_syntax::text::glob_match;
use buck2_repl_syntax::text::truncate_to_bytes;
use starlark::syntax::ast::Visibility;

use crate::bxl::starlark_defs::context::BxlContextCoreData;
use crate::repl::complete::private::PrivateBindings;
use crate::repl::complete::private::is_prelude_id;
use crate::repl::render::MAX_RENDER_TIME;
use crate::repl::render::MAX_STREAM_BYTES;
use crate::repl::render::RenderBudget;
use crate::repl::render::ReplFailure;
use crate::repl::render::preview;

/// Widest name column.
const MAX_NAME_WIDTH: usize = 32;

/// Widest type column.
const MAX_TYPE_WIDTH: usize = 24;

/// Longest type name shown.
const MAX_TYPE_BYTES: usize = 256;

/// One binding.
struct Row {
    name: String,
    type_name: String,
    preview: String,
    /// The module it was loaded from.
    from: Option<String>,
}

/// The listing of `:who`, one line per binding, sorted by name.
pub(crate) fn who(
    env: &BuckStarlarkModule<'_>,
    private: &PrivateBindings,
    globs: &[String],
    core: &BxlContextCoreData,
    budget: &RenderBudget<'_>,
) -> Result<String, ReplFailure> {
    let mut rows = Vec::new();
    // Each preview is short, but there may be many: the budget is checked for each.
    let mut stopped = false;
    for (name, visibility) in env.names_and_visibilities() {
        if !budget.go_on()? {
            stopped = true;
            break;
        }
        // Every session has them.
        if name == "ctx" || name == "_" {
            continue;
        }
        if !globs.is_empty() && !globs.iter().any(|g| glob_match(g, name)) {
            continue;
        }
        let (value, from) = match visibility {
            Visibility::Public => (env.get(name), None),
            Visibility::Private => match private.find(name) {
                Some((id, _)) if is_prelude_id(id) => continue,
                Some((id, value)) => (Some(value.add_to_heap(env.heap())), Some(id.to_owned())),
                // Defined in the session with a private name (`_x = 1`): the module does not
                // give out its value.
                None if name.starts_with('_') => {
                    rows.push(Row {
                        name: name.to_owned(),
                        type_name: "?".to_owned(),
                        preview: String::new(),
                        from: None,
                    });
                    continue;
                }
                // A `load` that failed.
                None => continue,
            },
        };
        // Declared by an input that failed before it assigned it.
        let Some(value) = value else {
            continue;
        };
        rows.push(Row {
            name: name.to_owned(),
            type_name: value.get_type().to_owned(),
            preview: preview(value, core, budget)?,
            from,
        });
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));

    if rows.is_empty() {
        return Ok(if globs.is_empty() {
            "(no bindings)".to_owned()
        } else {
            "(no bindings match)".to_owned()
        });
    }
    let name_width = rows
        .iter()
        .map(|r| r.name.len())
        .max()
        .unwrap_or(0)
        .min(MAX_NAME_WIDTH);
    let type_width = rows
        .iter()
        .map(|r| r.type_name.len())
        .max()
        .unwrap_or(0)
        .min(MAX_TYPE_WIDTH);
    let mut out = CappedString::new(MAX_STREAM_BYTES);
    for (i, row) in rows.iter().enumerate() {
        let mut line = format!(
            "{:<name_width$}  {:<type_width$}  {}",
            row.name,
            truncate_to_bytes(&row.type_name, MAX_TYPE_BYTES),
            row.preview
        );
        if let Some(from) = &row.from {
            line.push_str("  (from ");
            line.push_str(from);
            line.push(')');
        }
        let newline = if i == 0 { "" } else { "\n" };
        // A `CappedString` never fails.
        let _ignored = write!(out, "{newline}{}", line.trim_end());
    }
    let truncated = out.truncated();
    let mut out = out.into_string();
    if truncated {
        out.push_str(&format!(
            "\n… (the listing was cut after {} MiB)",
            MAX_STREAM_BYTES >> 20
        ));
    } else if stopped {
        out.push_str(&format!(
            "\n… (stopped after {} seconds: not every binding is listed)",
            MAX_RENDER_TIME.as_secs()
        ));
    }
    Ok(out)
}
