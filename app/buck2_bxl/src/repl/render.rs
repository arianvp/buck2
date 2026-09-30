/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Rendering of values and errors into bounded, `Send` text.
//!
//! Nothing here can panic or run unbounded on user values (INV-9): values are formatted with
//! `fmt::write` into a capped writer that stops the formatting at the cap or when the native
//! stack runs low, and values nested too deeply for the (recursive) formatter are detected
//! beforehand without recursion, in time bounded by the cap.
//!
//! One cost is not bounded here: the `Display` of a string builds its whole `repr` before
//! writing it. A top-level string is cut to the cap first; a huge string inside a container
//! still costs a few times its size, as it does anywhere in Starlark.

use std::collections::HashSet;
use std::fmt;
use std::fmt::Write;

use buck2_cli_proto::repl_error;
use buck2_error::starlark_error::NativeErrorHandling;
use buck2_error::starlark_error::from_starlark_with_options;
use buck2_repl_syntax::text::CappedString;
use buck2_repl_syntax::text::truncate_to_bytes;
use starlark::values::Value;
use starlark::values::dict::DictRef;
use starlark::values::list::ListRef;
use starlark::values::record::Record;
use starlark::values::string::StarlarkStr;
use starlark::values::structs::StructRef;
use starlark::values::tuple::TupleRef;

use crate::bxl::starlark_defs::artifacts::EnsuredArtifact;
use crate::bxl::starlark_defs::context::BxlContext;
use crate::bxl::starlark_defs::context::BxlContextCoreData;
use crate::bxl::starlark_defs::context::output::get_artifact_path_display;
use crate::bxl::starlark_defs::nodes::unconfigured::StarlarkTargetNode;

/// Most text in a `ReplValue` or a `ReplError`, leaving room for the rest of the message so that
/// every message stays within 64 KiB (INV-13).
pub(crate) const MAX_TEXT_BYTES: usize = (64 << 10) - 512;

/// Most bytes of a type name.
const MAX_TYPE_BYTES: usize = 256;

/// Deepest nesting of containers that is formatted: the formatter recurses on it.
const MAX_DISPLAY_DEPTH: usize = 256;

/// Formatting stops when less native stack than this is left: a backstop for values the depth
/// check does not see into (sets, user-defined providers, ...).
const RENDER_STACK_RESERVE: usize = 256 << 10;

/// Shown instead of a value that is nested too deeply to format.
pub(crate) const TOO_DEEP: &str = "<value nested too deeply to display>";

/// The rendering of a value.
pub(crate) struct RenderedValue {
    pub(crate) type_name: String,
    pub(crate) text: String,
    pub(crate) truncated: bool,
}

/// A failed request, rendered.
pub(crate) struct ReplFailure {
    pub(crate) kind: repl_error::Kind,
    /// Starts with `error: `, at most [`MAX_TEXT_BYTES`].
    pub(crate) message: String,
}

impl ReplFailure {
    /// `message`, which gets an `error: ` prefix unless it has one (as the rendering of an
    /// error with a source snippet does), cut to [`MAX_TEXT_BYTES`].
    pub(crate) fn new(kind: repl_error::Kind, message: &dyn fmt::Display) -> Self {
        const PREFIX: &str = "error: ";
        let mut out = CappedString::new(MAX_TEXT_BYTES - PREFIX.len());
        // A `CappedString` never fails; an error from a `Display` impl just ends the text.
        let _ignored = fmt::write(&mut out, format_args!("{message}"));
        let message = out.into_string();
        let message = if message.starts_with(PREFIX) {
            message
        } else {
            format!("{PREFIX}{message}")
        };
        ReplFailure { kind, message }
    }

    /// A buck2 error, rendered like the errors of other commands.
    pub(crate) fn from_buck2(kind: repl_error::Kind, e: &buck2_error::Error) -> Self {
        struct AsDebug<'a>(&'a buck2_error::Error);
        impl fmt::Display for AsDebug<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(self.0, f)
            }
        }
        Self::new(kind, &AsDebug(e))
    }

    /// An error from the parser or the evaluator. The message keeps the source snippet (the
    /// input is named `<repl:N>`).
    pub(crate) fn from_starlark(e: starlark::Error) -> Self {
        let kind = match e.kind() {
            starlark::ErrorKind::Parser(_) => repl_error::Kind::Syntax,
            starlark::ErrorKind::Native(_) => repl_error::Kind::Buck,
            starlark::ErrorKind::Internal(_) => repl_error::Kind::Internal,
            _ => repl_error::Kind::Eval,
        };
        let e = from_starlark_with_options(e, NativeErrorHandling::Unknown, false);
        Self::from_buck2(kind, &e)
    }

    /// Appends `note: {note}` on a line of its own.
    pub(crate) fn add_note(&mut self, note: &dyn fmt::Display) {
        let mut out = CappedString::new(MAX_TEXT_BYTES);
        let _ignored = fmt::write(
            &mut out,
            format_args!("{}\nnote: {note}", self.message.trim_end()),
        );
        self.message = out.into_string();
    }

    pub(crate) fn interrupted() -> Self {
        Self::new(repl_error::Kind::Interrupted, &"interrupted")
    }
}

/// Renders the value of an input with `{:#}`. `None` for `None`, which is not echoed.
///
/// `core` is the data of the session's `ctx`, which locates ensured artifacts.
pub(crate) fn render_echo(v: Value, core: &BxlContextCoreData) -> Option<RenderedValue> {
    if v.is_none() {
        return None;
    }
    let type_name = truncate_to_bytes(v.get_type(), MAX_TYPE_BYTES).to_owned();
    if let Some(text) = summary(v, core) {
        return Some(RenderedValue {
            type_name,
            text,
            truncated: false,
        });
    }
    if let Some(s) = v.unpack_str()
        && s.len() > MAX_TEXT_BYTES
    {
        // Only the start of it can be shown, and its `Display` would build all of its `repr`.
        let shown = StarlarkStr::repr(truncate_to_bytes(s, MAX_TEXT_BYTES));
        let shown = shown.strip_suffix('"').unwrap_or(&shown);
        return Some(RenderedValue {
            type_name,
            text: truncate_to_bytes(shown, MAX_TEXT_BYTES).to_owned(),
            truncated: true,
        });
    }
    if nesting_exceeds(v, MAX_DISPLAY_DEPTH) {
        return Some(RenderedValue {
            type_name,
            text: TOO_DEEP.to_owned(),
            truncated: false,
        });
    }
    let mut out = RenderWriter {
        buf: CappedString::new(MAX_TEXT_BYTES),
        stack_exhausted: false,
    };
    // Fails when the writer stops the formatting (see `RenderWriter`), or when a `Display` impl
    // fails; either way the text so far is kept.
    let _ignored = fmt::write(&mut out, format_args!("{v:#}"));
    if out.stack_exhausted {
        return Some(RenderedValue {
            type_name,
            text: TOO_DEEP.to_owned(),
            truncated: false,
        });
    }
    let truncated = out.buf.truncated();
    Some(RenderedValue {
        type_name,
        text: out.buf.into_string(),
        truncated,
    })
}

/// BXL types whose `Display` is their derived `Debug`, which is huge (a dump of the whole
/// context or node) and hides what matters: they are shown as a short summary instead.
const OPAQUE_BXL_TYPES: &[&str] = &[
    "bxl.AqueryContext",
    "bxl.AuditContext",
    "bxl.CqueryContext",
    "bxl.Filesystem",
    "bxl.OutputStream",
    "bxl.UqueryContext",
];

/// The summary of a top-level value of a BXL type with an unhelpful `Display`.
fn summary(v: Value, core: &BxlContextCoreData) -> Option<String> {
    let mut out = CappedString::new(MAX_TEXT_BYTES);
    // A `CappedString` never fails.
    let _ignored = if let Some(ensured) = v.downcast_ref::<EnsuredArtifact>() {
        // Its path, as `ctx.output.print` shows it: the artifact is materialized there once the
        // input is done. (Its `Display` is `<ensured ...>`, which cannot be used as a path.)
        let path = get_artifact_path_display(
            ensured.get_artifact_path(),
            ensured.abs(),
            core.project_fs(),
            core.artifact_fs(),
        )
        .ok()?;
        out.write_str(&path)
    } else if let Some(ctx) = v.downcast_ref::<BxlContext>() {
        match ctx.repl_cwd() {
            Some(cwd) => fmt::write(&mut out, format_args!("<bxl.Context cwd={cwd}>")),
            None => out.write_str("<bxl.Context>"),
        }
    } else if let Some(node) = v.downcast_ref::<StarlarkTargetNode>() {
        // Like the `Display` of configured target nodes.
        fmt::write(
            &mut out,
            format_args!("unconfigured_target_node(name = {}, ...)", node.0.label()),
        )
    } else if OPAQUE_BXL_TYPES.contains(&v.get_type()) {
        fmt::write(&mut out, format_args!("<{}>", v.get_type()))
    } else {
        return None;
    };
    Some(out.into_string())
}

/// A capped writer that ends the formatting (by failing) once its cap is reached, so that a huge
/// value (or a value that shares its parts exponentially often) costs no more than its first
/// [`MAX_TEXT_BYTES`], and when the native stack runs low.
///
/// Failing is safe: formatting code propagates `fmt::Error` with `?` (no `Display` impl in
/// buck2 or starlark-rust unwraps a write to its formatter), and `fmt::write` returns it.
struct RenderWriter {
    buf: CappedString,
    stack_exhausted: bool,
}

impl Write for RenderWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if stacker::remaining_stack().is_some_and(|left| left < RENDER_STACK_RESERVE) {
            self.stack_exhausted = true;
            return Err(fmt::Error);
        }
        self.buf.write_str(s)?;
        if self.buf.truncated() {
            Err(fmt::Error)
        } else {
            Ok(())
        }
    }
}

/// Most values the depth check visits. The formatter writes at least one byte before every value
/// it visits below the top one (an opening bracket, a separator or a key), and it stops at
/// [`MAX_TEXT_BYTES`], so it never gets further than this.
const MAX_VISITS: usize = MAX_TEXT_BYTES + 1;

/// The children of a container are fetched this many at a time, so that the check holds at most
/// one chunk per container on its path, however large the containers are.
const CHILDREN_CHUNK: usize = 1024;

/// Up to `max` of the values that formatting `v` recurses into, from the `from`th on, in the
/// order the formatter visits them; `None` if `v` is not a container.
fn children<'v>(v: Value<'v>, from: usize, max: usize) -> Option<Vec<Value<'v>>> {
    fn chunk<'v>(it: impl Iterator<Item = Value<'v>>, from: usize, max: usize) -> Vec<Value<'v>> {
        it.skip(from).take(max).collect()
    }
    if let Some(list) = ListRef::from_value(v) {
        Some(chunk(list.iter(), from, max))
    } else if let Some(tuple) = TupleRef::from_value(v) {
        Some(chunk(tuple.iter(), from, max))
    } else if let Some(dict) = DictRef::from_value(v) {
        Some(chunk(dict.iter().flat_map(|(k, v)| [k, v]), from, max))
    } else if let Some(s) = StructRef::from_value(v) {
        Some(chunk(s.iter().map(|(_, v)| v), from, max))
    } else {
        Record::from_value(v).map(|r| chunk(r.iter().map(|(_, v)| v), from, max))
    }
}

/// A container on the path of the depth check.
struct Frame<'v> {
    value: Value<'v>,
    /// Children fetched and not visited yet, the next one last.
    pending: Vec<Value<'v>>,
    /// How many children have been fetched.
    fetched: usize,
    /// Every child has been fetched.
    complete: bool,
}

impl<'v> Frame<'v> {
    fn new(value: Value<'v>, first: Vec<Value<'v>>) -> Self {
        let mut frame = Frame {
            value,
            pending: Vec::new(),
            fetched: 0,
            complete: false,
        };
        frame.add(first);
        frame
    }

    fn add(&mut self, mut chunk: Vec<Value<'v>>) {
        self.complete = chunk.len() < CHILDREN_CHUNK;
        self.fetched = self.fetched.saturating_add(chunk.len());
        chunk.reverse();
        self.pending = chunk;
    }

    fn next_child(&mut self) -> Option<Value<'v>> {
        if self.pending.is_empty() && !self.complete {
            let chunk = children(self.value, self.fetched, CHILDREN_CHUNK).unwrap_or_default();
            self.add(chunk);
        }
        self.pending.pop()
    }
}

/// Whether formatting `root` recurses more than `limit` containers deep, checked without
/// recursion.
///
/// Walks the value the way the formatter does: depth first, children in order, and a value that
/// is already on the path is not entered again (the formatter shows it as `[...]`). The walk
/// stops where the formatter would have stopped at its size cap ([`MAX_VISITS`]), so it costs
/// time and memory bounded by the size of the rendering, not by the size of the value (INV-9):
/// the part of a value past the cap is never formatted, so its depth does not matter.
///
/// Values the walk does not look into (sets, providers, ...) are left to the stack check of
/// [`RenderWriter`].
fn nesting_exceeds(root: Value, limit: usize) -> bool {
    let Some(first) = children(root, 0, CHILDREN_CHUNK) else {
        return false;
    };
    let mut on_path = HashSet::from([root.identity()]);
    let mut path = vec![Frame::new(root, first)];
    let mut visits = 0usize;
    while let Some(frame) = path.last_mut() {
        let Some(child) = frame.next_child() else {
            on_path.remove(&frame.value.identity());
            path.pop();
            continue;
        };
        visits += 1;
        if visits > MAX_VISITS {
            return false;
        }
        if on_path.contains(&child.identity()) {
            continue;
        }
        let Some(grandchildren) = children(child, 0, CHILDREN_CHUNK) else {
            continue;
        };
        if path.len() >= limit {
            return true;
        }
        on_path.insert(child.identity());
        path.push(Frame::new(child, grandchildren));
    }
    false
}
