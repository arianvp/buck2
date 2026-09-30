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
//! beforehand without recursion.

use std::collections::HashMap;
use std::fmt;
use std::fmt::Write;

use buck2_cli_proto::repl_error;
use buck2_error::starlark_error::NativeErrorHandling;
use buck2_error::starlark_error::from_starlark_with_options;
use buck2_repl_syntax::text::CappedString;
use buck2_repl_syntax::text::truncate_to_bytes;
use starlark::values::Value;
use starlark::values::ValueIdentity;
use starlark::values::dict::DictRef;
use starlark::values::list::ListRef;
use starlark::values::record::Record;
use starlark::values::structs::StructRef;
use starlark::values::tuple::TupleRef;

use crate::bxl::starlark_defs::context::BxlContext;
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
pub(crate) fn render_echo(v: Value) -> Option<RenderedValue> {
    if v.is_none() {
        return None;
    }
    let type_name = truncate_to_bytes(v.get_type(), MAX_TYPE_BYTES).to_owned();
    if let Some(text) = summary(v) {
        return Some(RenderedValue {
            type_name,
            text,
            truncated: false,
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
fn summary(v: Value) -> Option<String> {
    let mut out = CappedString::new(MAX_TEXT_BYTES);
    // A `CappedString` never fails.
    let _ignored = if let Some(ctx) = v.downcast_ref::<BxlContext>() {
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

/// The containers directly inside `v` (the values its formatting recurses into), or `None` if
/// `v` is not a container.
fn children<'v>(v: Value<'v>) -> Option<Vec<Value<'v>>> {
    fn keep<'v>(it: impl Iterator<Item = Value<'v>>) -> Vec<Value<'v>> {
        it.filter(|c| is_container(*c)).collect()
    }
    if let Some(list) = ListRef::from_value(v) {
        Some(keep(list.iter()))
    } else if let Some(tuple) = TupleRef::from_value(v) {
        Some(keep(tuple.iter()))
    } else if let Some(dict) = DictRef::from_value(v) {
        Some(keep(dict.iter().flat_map(|(k, v)| [k, v])))
    } else if let Some(s) = StructRef::from_value(v) {
        Some(keep(s.iter().map(|(_, v)| v)))
    } else {
        Record::from_value(v).map(|r| keep(r.iter().map(|(_, v)| v)))
    }
}

fn is_container(v: Value) -> bool {
    ListRef::from_value(v).is_some()
        || TupleRef::from_value(v).is_some()
        || DictRef::from_value(v).is_some()
        || StructRef::from_value(v).is_some()
        || Record::from_value(v).is_some()
}

/// Whether formatting `root` may recurse more than `limit` containers deep, checked without
/// recursion, in time linear in the size of the value.
///
/// The formatter follows simple paths (it cuts a cycle at a value it is already inside). The
/// longest simple path is bounded by the longest path through the strongly connected components
/// of the containers, each counting as its size: exact for acyclic values (so no false positives
/// on them), an over-estimate only for cyclic ones.
fn nesting_exceeds(root: Value, limit: usize) -> bool {
    struct Node<'v> {
        index: usize,
        lowlink: usize,
        on_stack: bool,
        /// Set once its component is complete.
        component: Option<usize>,
        children: Vec<ValueIdentity<'v>>,
    }
    struct Frame<'v> {
        id: ValueIdentity<'v>,
        children: Vec<Value<'v>>,
        next: usize,
    }
    struct Search<'v> {
        nodes: HashMap<ValueIdentity<'v>, Node<'v>>,
        /// Tarjan's stack.
        stack: Vec<ValueIdentity<'v>>,
        /// The depth-first path: a simple path, which the formatter also follows.
        frames: Vec<Frame<'v>>,
        /// For each complete component: the longest path from it, counting components by size.
        bounds: Vec<usize>,
    }
    impl<'v> Search<'v> {
        fn enter(&mut self, v: Value<'v>, children: Vec<Value<'v>>) {
            let id = v.identity();
            let index = self.nodes.len();
            self.nodes.insert(
                id,
                Node {
                    index,
                    lowlink: index,
                    on_stack: true,
                    component: None,
                    children: children.iter().map(|c| c.identity()).collect(),
                },
            );
            self.stack.push(id);
            self.frames.push(Frame {
                id,
                children,
                next: 0,
            });
        }

        fn lower(&mut self, id: ValueIdentity<'v>, to: usize) {
            if let Some(node) = self.nodes.get_mut(&id) {
                node.lowlink = node.lowlink.min(to);
            }
        }

        /// Completes the component rooted at `root`; returns its bound.
        fn complete(&mut self, root: ValueIdentity<'v>) -> usize {
            let component = self.bounds.len();
            let mut members = Vec::new();
            while let Some(member) = self.stack.pop() {
                if let Some(node) = self.nodes.get_mut(&member) {
                    node.on_stack = false;
                    node.component = Some(component);
                }
                members.push(member);
                if member == root {
                    break;
                }
            }
            // Components complete in reverse topological order: every other component reached
            // from this one is complete.
            let mut reach = 0;
            for member in &members {
                let Some(node) = self.nodes.get(member) else {
                    continue;
                };
                for child in &node.children {
                    if let Some(c) = self.nodes.get(child).and_then(|n| n.component) {
                        if c != component {
                            reach = reach.max(self.bounds.get(c).copied().unwrap_or(0));
                        }
                    }
                }
            }
            let bound = members.len().saturating_add(reach);
            self.bounds.push(bound);
            bound
        }
    }

    let Some(root_children) = children(root) else {
        return false;
    };
    // Tarjan's algorithm, iteratively.
    let mut search = Search {
        nodes: HashMap::new(),
        stack: Vec::new(),
        frames: Vec::new(),
        bounds: Vec::new(),
    };
    search.enter(root, root_children);
    loop {
        if search.frames.len() > limit {
            return true;
        }
        let Some(frame) = search.frames.last_mut() else {
            return false;
        };
        let parent = frame.id;
        match frame.children.get(frame.next).copied() {
            Some(child) => {
                frame.next += 1;
                match search.nodes.get(&child.identity()) {
                    None => {
                        let grandchildren = children(child).unwrap_or_default();
                        search.enter(child, grandchildren);
                    }
                    Some(node) if node.on_stack => {
                        let index = node.index;
                        search.lower(parent, index);
                    }
                    // In a complete component.
                    Some(_) => {}
                }
            }
            None => {
                search.frames.pop();
                let Some((index, lowlink)) =
                    search.nodes.get(&parent).map(|n| (n.index, n.lowlink))
                else {
                    return true;
                };
                if let Some(grandparent) = search.frames.last().map(|f| f.id) {
                    search.lower(grandparent, lowlink);
                }
                if lowlink == index && search.complete(parent) > limit {
                    return true;
                }
            }
        }
    }
}
