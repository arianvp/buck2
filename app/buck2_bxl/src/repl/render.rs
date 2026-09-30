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
//! still costs a few times its size, as it does anywhere in Starlark. An int is worse (its
//! conversion to decimal takes time that grows faster than its size), so an int of more than
//! [`MAX_SHOWN_INT_BITS`] is not formatted where the depth check sees it: it is shown as its
//! size.
//!
//! `:doc` asks values for their documentation, which formats nothing of the session's values
//! except for its functions and namespaces, which are documented otherwise (see
//! [`documentation`]). The documentation of a function loaded from a file formats the default
//! values of its parameters with no bound, as `buck2 docs starlark` and the LSP do; only the
//! file can give it large ones.
//!
//! The value of an input is echoed ([`RenderMode::Echo`]); meta-commands render it in other
//! modes (`:type`, `:print`, `:json`, `:doc`).

use std::collections::HashSet;
use std::fmt;
use std::fmt::Write;
use std::time::Duration;
use std::time::Instant;

use buck2_cli_proto::repl_error;
use buck2_error::starlark_error::NativeErrorHandling;
use buck2_error::starlark_error::from_starlark_with_options;
use buck2_repl_syntax::text::CappedString;
use buck2_repl_syntax::text::truncate_to_bytes;
use num_bigint::BigInt;
use starlark::docs::DocItem;
use starlark::docs::DocMember;
use starlark::docs::DocModule;
use starlark::docs::DocProperty;
use starlark::docs::DocString;
use starlark::docs::DocStringKind;
use starlark::docs::markdown::render_doc_item_no_link;
use starlark::typing::Ty;
use starlark::values::Heap;
use starlark::values::UnpackValue;
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
use crate::repl::complete::types::TypeIndex;
use crate::repl::docstrings::Docstrings;

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

/// An int of more bits than this is not formatted (nor converted to JSON): its `Display` (and
/// its JSON form) converts the whole number to decimal before writing any of it, in time that
/// grows faster than its size, which neither the cap nor the budget can stop (an int of 64
/// million bits takes half a minute). This many bits are about 79000 digits, more than an echo
/// shows.
const MAX_SHOWN_INT_BITS: u64 = 256 << 10;

/// Deepest nesting of containers the check before a conversion to JSON looks into: the
/// serializer runs out of native stack (and stops) long before.
const MAX_JSON_DEPTH: usize = 1024;

/// Longest JSON form of a value (`ReplValue.json`, for `buck2 repl --json`). It is the one part of
/// a message that may make it longer than 64 KiB.
pub(crate) const MAX_JSON_BYTES: usize = 1 << 20;

/// Most text `:print`, `:json` and `:doc` send. It goes to the client's stdout as output, in
/// chunks.
pub(crate) const MAX_STREAM_BYTES: usize = 16 << 20;

/// Longest a value may take to render. Formatting is native code that the cancellation of the
/// request does not stop by itself, and a value can cost much more than the size of its
/// rendering: every level of nesting in the pretty (`{:#}`) form adds work to every byte below
/// it, and some values (sets, providers, ...) are not looked into beforehand.
pub(crate) const MAX_RENDER_TIME: Duration = Duration::from_secs(10);

/// `:print` shows a value nested deeper than this on one line (`{}`, which costs no more than
/// its size) rather than pretty-printed: 16 MiB pretty-printed 16 deep take about 6 seconds.
const MAX_PRETTY_DEPTH: usize = 16;

/// The same for the echo of a value, which is much shorter.
const MAX_ECHO_PRETTY_DEPTH: usize = 64;

/// The budget is checked after this many writes (or values visited by the depth check, times
/// [`CHILDREN_CHUNK`]).
const BUDGET_CHECK_INTERVAL: u32 = 256;

/// Longest heading of `:doc`: the expression as typed.
const MAX_DOC_NAME_BYTES: usize = 80;

/// Longest name of a function (`<repl:3>.f`) that `:doc` looks up a docstring for.
const MAX_FUNCTION_NAME_BYTES: usize = 1 << 10;

/// The type checker's type of a struct is made of the types of its fields, recursively: its type
/// is not computed when the struct has more than this many fields in all (counting shared parts
/// once per use), or ...
const MAX_TYPE_FIELDS: usize = 4096;

/// ... when its structs nest deeper than this.
const MAX_TYPE_DEPTH: usize = 64;

/// How the value of an input is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RenderMode {
    /// An input: `{:#}`, cut at [`MAX_TEXT_BYTES`]; `None` is not shown.
    Echo,
    /// `:print`: all of `{:#}` (up to [`MAX_STREAM_BYTES`]), sent as output.
    Print,
    /// `:json`: pretty JSON, sent as output.
    Json,
    /// `:type`: the type of the value.
    Type,
    /// `:doc`: the documentation of the value, or of its type, as Markdown, sent as output.
    Doc,
}

impl RenderMode {
    /// Whether the value becomes `_` (unless it is `None`).
    pub(crate) fn binds_last_value(self) -> bool {
        match self {
            RenderMode::Echo | RenderMode::Print | RenderMode::Json => true,
            RenderMode::Type | RenderMode::Doc => false,
        }
    }
}

/// What an input shows.
pub(crate) enum Rendered {
    /// Nothing (the value is `None`).
    Nothing,
    /// A value, as a `ReplValue`.
    Value(RenderedValue),
    /// Text that may be long: sent to the client's stdout as output.
    Text(RenderedText),
}

/// The rendering of a value.
pub(crate) struct RenderedValue {
    pub(crate) type_name: String,
    pub(crate) text: String,
    pub(crate) truncated: bool,
    /// The value as compact JSON, when asked for (`buck2 repl --json`) and the value has a JSON
    /// form of at most [`MAX_JSON_BYTES`].
    pub(crate) json: Option<String>,
}

/// Text for the client's stdout.
pub(crate) struct RenderedText {
    pub(crate) text: String,
    /// Why the text is incomplete, if it is (a warning shown after it).
    pub(crate) incomplete: Option<String>,
}

impl RenderedText {
    fn new(text: String, truncated: bool) -> Self {
        RenderedText {
            text,
            incomplete: truncated
                .then(|| format!("the output was cut after {} MiB", MAX_STREAM_BYTES >> 20)),
        }
    }
}

/// When rendering stops early: when the request is interrupted, or when it has taken
/// [`MAX_RENDER_TIME`].
pub(crate) struct RenderBudget<'a> {
    deadline: Instant,
    cancelled: &'a dyn Fn() -> bool,
}

impl<'a> RenderBudget<'a> {
    /// A budget that starts now. `cancelled` tells whether the request was interrupted.
    pub(crate) fn new(cancelled: &'a dyn Fn() -> bool) -> Self {
        RenderBudget {
            deadline: Instant::now() + MAX_RENDER_TIME,
            cancelled,
        }
    }

    /// Whether work that renders many values goes on: fails when the request was interrupted,
    /// `false` when the time is up.
    pub(crate) fn go_on(&self) -> Result<bool, ReplFailure> {
        match self.check() {
            Ok(()) => Ok(true),
            Err(Stop::Interrupted) => Err(ReplFailure::interrupted()),
            Err(Stop::TimeUp) => Ok(false),
        }
    }

    fn check(&self) -> Result<(), Stop> {
        if (self.cancelled)() {
            Err(Stop::Interrupted)
        } else if Instant::now() >= self.deadline {
            Err(Stop::TimeUp)
        } else {
            Ok(())
        }
    }
}

/// Why rendering stopped early.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    Interrupted,
    TimeUp,
}

/// Rendered text, before it becomes a value or output.
struct Rendering {
    text: String,
    /// Cut at the cap.
    truncated: bool,
    /// Stopped at [`MAX_RENDER_TIME`].
    timed_out: bool,
}

impl Rendering {
    fn complete(text: String) -> Self {
        Rendering {
            text,
            truncated: false,
            timed_out: false,
        }
    }

    fn into_value(self, type_name: String) -> RenderedValue {
        RenderedValue {
            type_name,
            text: self.text,
            truncated: self.truncated || self.timed_out,
            json: None,
        }
    }

    fn into_text(self) -> RenderedText {
        let timed_out = self.timed_out;
        let mut text = RenderedText::new(self.text, self.truncated);
        if timed_out {
            text.incomplete = Some(format!(
                "rendering was stopped after {} seconds: the output is incomplete",
                MAX_RENDER_TIME.as_secs()
            ));
        }
        text
    }
}

/// What rendering may need besides the value.
pub(crate) struct RenderContext<'a, 'v> {
    /// The data of the session's `ctx`, which locates ensured artifacts.
    pub(crate) core: &'a BxlContextCoreData,
    /// The docstrings of the functions defined in the session, for `:doc`.
    pub(crate) docstrings: &'a Docstrings,
    pub(crate) heap: Heap<'v>,
    pub(crate) budget: RenderBudget<'a>,
    /// The documentation of the types of the globals (needed by `:doc` only).
    pub(crate) types: Option<&'a TypeIndex>,
    /// The code as typed, which names the value in the heading of `:doc`.
    pub(crate) code: &'a str,
    /// The echo of a value carries its JSON form too (`buck2 repl --json`).
    pub(crate) json_values: bool,
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

/// Renders a value in `mode`. Fails for `:json` of a value that has no JSON form, and when the
/// request is interrupted.
pub(crate) fn render<'v>(
    v: Value<'v>,
    mode: RenderMode,
    cx: &RenderContext<'_, 'v>,
) -> Result<Rendered, ReplFailure> {
    let interrupted = |_: Stop| ReplFailure::interrupted();
    let type_name = || truncate_to_bytes(v.get_type(), MAX_TYPE_BYTES).to_owned();
    Ok(match mode {
        RenderMode::Echo if v.is_none() => Rendered::Nothing,
        RenderMode::Echo => {
            let mut value = render_value(
                v,
                cx.core,
                MAX_TEXT_BYTES,
                MAX_ECHO_PRETTY_DEPTH,
                &cx.budget,
            )
            .map_err(interrupted)?
            .into_value(type_name());
            if cx.json_values {
                value.json = json_form(v, cx.core, &cx.budget);
            }
            Rendered::Value(value)
        }
        RenderMode::Print => Rendered::Text(
            render_value(v, cx.core, MAX_STREAM_BYTES, MAX_PRETTY_DEPTH, &cx.budget)
                .map_err(interrupted)?
                .into_text(),
        ),
        RenderMode::Json => Rendered::Text(render_json(v, &cx.budget)?),
        RenderMode::Type => Rendered::Value(render_type(v, cx.heap)),
        RenderMode::Doc => Rendered::Text(render_doc(v, cx)),
    })
}

/// Renders a value with `{:#}` (with `{}` if it nests deeper than `pretty_depth`), cut at `cap`
/// bytes. A value of a BXL type whose `Display` is a dump of its internals is shown as a summary
/// instead (see [`summary`]). Fails only when the request is interrupted.
fn render_value(
    v: Value,
    core: &BxlContextCoreData,
    cap: usize,
    pretty_depth: usize,
    budget: &RenderBudget<'_>,
) -> Result<Rendering, Stop> {
    if let Some(text) = summary(v, core) {
        return Ok(Rendering::complete(text));
    }
    if let Some(bits) = huge_int_bits(v) {
        return Ok(Rendering::complete(format!(
            "<int of {bits} bits: too large to display>"
        )));
    }
    if let Some(s) = v.unpack_str()
        && s.len() > cap
    {
        // Only the start of it can be shown, and its `Display` would build all of its `repr`.
        let shown = StarlarkStr::repr(truncate_to_bytes(s, cap));
        let shown = shown.strip_suffix('"').unwrap_or(&shown);
        return Ok(Rendering {
            text: truncate_to_bytes(shown, cap).to_owned(),
            truncated: true,
            timed_out: false,
        });
    }
    // The formatter writes at least one byte before every value it visits below the top one,
    // and it stops at the cap, so it never visits more than this.
    let max_visits = cap.saturating_add(1);
    let depth = match nesting_depth(v, MAX_DISPLAY_DEPTH, max_visits, budget) {
        Ok(Nesting {
            huge_int: Some(bits),
            ..
        }) => {
            return Ok(Rendering::complete(format!(
                "<value holding an int of {bits} bits: too large to display>"
            )));
        }
        Ok(Nesting { depth, .. }) => depth,
        Err(Stop::Interrupted) => return Err(Stop::Interrupted),
        // Nothing was rendered yet.
        Err(Stop::TimeUp) => {
            return Ok(Rendering {
                text: String::new(),
                truncated: false,
                timed_out: true,
            });
        }
    };
    if depth > MAX_DISPLAY_DEPTH {
        return Ok(Rendering::complete(TOO_DEEP.to_owned()));
    }
    let mut out = RenderWriter {
        buf: CappedString::new(cap),
        stack_exhausted: false,
        budget,
        writes: 0,
        stopped: None,
    };
    // Fails when the writer stops the formatting (see `RenderWriter`), or when a `Display` impl
    // fails; either way the text so far is kept.
    let _ignored = if depth <= pretty_depth {
        fmt::write(&mut out, format_args!("{v:#}"))
    } else {
        fmt::write(&mut out, format_args!("{v}"))
    };
    if out.stack_exhausted {
        return Ok(Rendering::complete(TOO_DEEP.to_owned()));
    }
    if out.stopped == Some(Stop::Interrupted) {
        return Err(Stop::Interrupted);
    }
    Ok(Rendering {
        truncated: out.buf.truncated(),
        timed_out: out.stopped == Some(Stop::TimeUp),
        text: out.buf.into_string(),
    })
}

/// Longest preview of a value (`:who`).
const PREVIEW_BYTES: usize = 60;

/// The start of the rendering of a value, on one line, cut at [`PREVIEW_BYTES`] (then it ends
/// with `…`). Fails only when the request is interrupted.
pub(crate) fn preview(
    v: Value,
    core: &BxlContextCoreData,
    budget: &RenderBudget<'_>,
) -> Result<String, ReplFailure> {
    let rendering =
        render_value(v, core, PREVIEW_BYTES, 0, budget).map_err(|_| ReplFailure::interrupted())?;
    let mut text: String = rendering
        .text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if rendering.truncated || rendering.timed_out {
        text.push('…');
    }
    Ok(text)
}

/// `:type`: the type the type checker gives the value (`list[str]`, `def(x: int) -> str`, ...),
/// and what `type()` returns when that differs.
fn render_type<'v>(v: Value<'v>, heap: Heap<'v>) -> RenderedValue {
    let type_name = truncate_to_bytes(v.get_type(), MAX_TYPE_BYTES).to_owned();
    let mut out = CappedString::new(MAX_TEXT_BYTES);
    if type_too_large(v, heap) {
        let _ignored = fmt::write(
            &mut out,
            format_args!("{type_name}  # its fields are too many or nest too deeply to show"),
        );
        return RenderedValue {
            type_name,
            text: out.into_string(),
            truncated: false,
            json: None,
        };
    }
    // A `CappedString` never fails; an error from a `Display` impl just ends the text.
    let _ignored = fmt::write(&mut out, format_args!("{}", Ty::of_value(v)));
    if out.as_str() != type_name {
        let _ignored = fmt::write(&mut out, format_args!("  # type() is \"{type_name}\""));
    }
    let truncated = out.truncated();
    let mut text = out.into_string();
    if truncated {
        // Said here: the client's note on a cut value points to `:print _`, and `:type` does not
        // bind `_`.
        const CUT: &str = "… (the type is cut at 64 KiB)";
        text = format!(
            "{}{CUT}",
            truncate_to_bytes(&text, MAX_TEXT_BYTES - CUT.len())
        );
    }
    RenderedValue {
        type_name,
        text,
        truncated: false,
        json: None,
    }
}

/// Whether the type checker's type of `v` is too large to compute safely: the type of a struct
/// (or a namespace) is made of the types of its fields, computed recursively, so it is as large as
/// the value's tree of structs, which can be very deep or share its parts exponentially often.
/// Checked without recursion, in time bounded by [`MAX_TYPE_FIELDS`].
fn type_too_large<'v>(v: Value<'v>, heap: Heap<'v>) -> bool {
    let mut stack = vec![(v, 0usize)];
    let mut fields = 0usize;
    while let Some((v, depth)) = stack.pop() {
        // One more field than the limit allows is enough to know.
        let room = (MAX_TYPE_FIELDS + 1).saturating_sub(fields);
        let children: Vec<Value<'v>> = if let Some(s) = StructRef::from_value(v) {
            s.iter().map(|(_, field)| field).take(room).collect()
        } else if v.get_type() == "namespace" {
            v.dir_attr()
                .iter()
                .take(room)
                .filter_map(|name| v.get_attr(name, heap).ok().flatten())
                .collect()
        } else {
            continue;
        };
        fields = fields.saturating_add(children.len());
        if fields > MAX_TYPE_FIELDS || depth >= MAX_TYPE_DEPTH {
            return true;
        }
        stack.extend(children.into_iter().map(|child| (child, depth + 1)));
    }
    false
}

/// `:json`: the value as pretty JSON. Fails if the value (or a value in it) has no JSON form,
/// or when the request is interrupted.
fn render_json(v: Value, budget: &RenderBudget<'_>) -> Result<RenderedText, ReplFailure> {
    let no_json = |why: &dyn fmt::Display| {
        ReplFailure::new(
            repl_error::Kind::Eval,
            &format_args!(
                "the value (of type `{}`) cannot be converted to JSON: {why}",
                truncate_to_bytes(v.get_type(), MAX_TYPE_BYTES)
            ),
        )
    };
    match json_obstacle(v, MAX_STREAM_BYTES, budget) {
        Ok(None) => {}
        Ok(Some(why)) => return Err(no_json(&why)),
        Err(Stop::Interrupted) => return Err(ReplFailure::interrupted()),
        Err(Stop::TimeUp) => {
            return Ok(Rendering {
                text: String::new(),
                truncated: false,
                timed_out: true,
            }
            .into_text());
        }
    }
    let mut out = CappedBytes::new(MAX_STREAM_BYTES, budget);
    // The serializer of Starlark values stops at cycles, and before it runs out of native stack
    // in the containers it serializes itself; a `cmd_args` serializes its `Display` (which
    // recurses on the `cmd_args` in it) with a check of its own. The writer stops it (by failing)
    // at the cap, when the budget is spent, and when the native stack runs low anyway.
    let result = serde_json::to_writer_pretty(&mut out, &v);
    if out.stopped == Some(Stop::Interrupted) {
        return Err(ReplFailure::interrupted());
    }
    if out.stack_exhausted {
        return Err(no_json(&"it is nested too deeply"));
    }
    if let Err(e) = result
        && !out.full
        && out.stopped.is_none()
    {
        return Err(no_json(&e));
    }
    let text = match String::from_utf8(out.buf) {
        Ok(text) => text,
        // Cut in the middle of a character.
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    Ok(Rendering {
        text,
        truncated: out.full,
        timed_out: out.stopped == Some(Stop::TimeUp),
    }
    .into_text())
}

/// Why `v` is not converted to JSON, found before trying, in time bounded by `cap` (the most
/// bytes the conversion writes): it holds an int too large to convert (see
/// [`MAX_SHOWN_INT_BITS`]), or its containers nest too deeply. Fails when the budget is spent.
fn json_obstacle(v: Value, cap: usize, budget: &RenderBudget<'_>) -> Result<Option<String>, Stop> {
    if let Some(bits) = huge_int_bits(v) {
        return Ok(Some(format!("an int of {bits} bits is too large")));
    }
    // Like the formatter, the serializer writes at least one byte before every value it visits
    // below the top one.
    let nesting = nesting_depth(v, MAX_JSON_DEPTH, cap.saturating_add(1), budget)?;
    Ok(if let Some(bits) = nesting.huge_int {
        Some(format!(
            "it holds an int of {bits} bits, which is too large"
        ))
    } else if nesting.depth > MAX_JSON_DEPTH {
        Some("it is nested too deeply".to_owned())
    } else {
        None
    })
}

/// The JSON form of a value for `buck2 repl --json`: compact JSON, if the value has one (as for
/// `:json`) of at most [`MAX_JSON_BYTES`], and if it is done within the budget. An ensured
/// artifact is its path (as it is echoed); the other values shown as a summary (`ctx`, target
/// nodes, ...) have none.
fn json_form(v: Value, core: &BxlContextCoreData, budget: &RenderBudget<'_>) -> Option<String> {
    if v.downcast_ref::<EnsuredArtifact>().is_some() {
        return summary(v, core).and_then(|path| serde_json::to_string(&path).ok());
    }
    if summary(v, core).is_some() {
        return None;
    }
    if !matches!(json_obstacle(v, MAX_JSON_BYTES, budget), Ok(None)) {
        return None;
    }
    let mut out = CappedBytes::new(MAX_JSON_BYTES, budget);
    // Stopped as for `:json` (see `render_json`).
    serde_json::to_writer(&mut out, &v).ok()?;
    if out.full || out.stopped.is_some() || out.stack_exhausted {
        return None;
    }
    String::from_utf8(out.buf).ok()
}

/// A byte buffer that keeps at most `cap` bytes, and fails the write that reaches the cap, or
/// that finds the budget spent or the native stack low (the backstop of [`RenderWriter`] too),
/// which stops the serializer writing into it. Every later write fails too.
struct CappedBytes<'a> {
    buf: Vec<u8>,
    cap: usize,
    full: bool,
    stack_exhausted: bool,
    budget: &'a RenderBudget<'a>,
    writes: u32,
    stopped: Option<Stop>,
}

impl<'a> CappedBytes<'a> {
    fn new(cap: usize, budget: &'a RenderBudget<'a>) -> Self {
        CappedBytes {
            buf: Vec::new(),
            cap,
            full: false,
            stack_exhausted: false,
            budget,
            writes: 0,
            stopped: None,
        }
    }
}

impl std::io::Write for CappedBytes<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.full || self.stack_exhausted || self.stopped.is_some() {
            return Err(std::io::Error::other("rendering stopped"));
        }
        if stacker::remaining_stack().is_some_and(|left| left < RENDER_STACK_RESERVE) {
            self.stack_exhausted = true;
            return Err(std::io::Error::other("value nested too deeply"));
        }
        self.writes = self.writes.wrapping_add(1);
        if self.writes.is_multiple_of(BUDGET_CHECK_INTERVAL)
            && let Err(stop) = self.budget.check()
        {
            self.stopped = Some(stop);
            return Err(std::io::Error::other("rendering stopped"));
        }
        let room = self.cap.saturating_sub(self.buf.len());
        if data.len() > room {
            self.buf
                .extend_from_slice(data.get(..room).unwrap_or_default());
            self.full = true;
            return Err(std::io::Error::other("output limit reached"));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `:doc`: the documentation of the value (a function, a type, a namespace), or of its type if
/// the value is an instance, as Markdown.
fn render_doc<'v>(v: Value<'v>, cx: &RenderContext<'_, 'v>) -> RenderedText {
    let code = cx.code.trim();
    let code_name = match code.lines().next() {
        Some(first) if first.len() <= MAX_DOC_NAME_BYTES && first.len() == code.len() => first,
        _ => "value",
    };
    // The documentation of an instance is its type, which may be too large to compute.
    let documentation = if type_too_large(v, cx.heap) {
        None
    } else {
        Some(documentation(v, cx))
    };
    let (name, item) = match documentation {
        Some(Documentation::Own(item)) => (code_name.to_owned(), *item),
        Some(Documentation::Instance(property)) => {
            // An instance: its documentation only names its type.
            let type_name = property
                .typ
                .as_name()
                .unwrap_or_else(|| v.get_type())
                .to_owned();
            match type_documentation(v, &type_name, cx) {
                Some(item) => (type_name, item),
                None => (
                    code_name.to_owned(),
                    DocItem::Member(DocMember::Property(property)),
                ),
            }
        }
        None => match type_documentation(v, v.get_type(), cx) {
            Some(item) => (v.get_type().to_owned(), item),
            None => {
                return RenderedText::new(
                    format!(
                        "no documentation for `{code_name}` (of type `{}`)",
                        truncate_to_bytes(v.get_type(), MAX_TYPE_BYTES)
                    ),
                    false,
                );
            }
        },
    };
    let mut text = render_doc_item_no_link(&name, &item);
    let truncated = text.len() > MAX_STREAM_BYTES;
    if truncated {
        text = truncate_to_bytes(&text, MAX_STREAM_BYTES).to_owned();
    }
    RenderedText::new(text, truncated)
}

/// What `:doc` knows of a value.
enum Documentation {
    /// The value's own documentation: a function's, a type's, a namespace's, ... (boxed: it is
    /// large).
    Own(Box<DocItem>),
    /// The value is an instance: its documentation only names its type.
    Instance(DocProperty),
}

impl Documentation {
    fn into_item(self) -> DocItem {
        match self {
            Documentation::Own(item) => *item,
            Documentation::Instance(property) => DocItem::Member(DocMember::Property(property)),
        }
    }
}

/// The documentation of `v`: `Value::documentation`, except for two kinds of values defined in
/// the session (not frozen), whose documentation formats other values of the session:
///
/// - A function (a `def` or a `lambda`). Its documentation formats the default values of its
///   parameters with `repr`, which recurses on them with no stack check and takes as long as
///   they are large: a default value nested 100000 deep overflows the stack (and aborts the
///   daemon), one that shares its parts exponentially often never finishes. It is documented by
///   its type instead, which shows default values as `...`, and by the docstring recorded when
///   it was defined ([`Docstrings`]).
/// - A namespace, whose documentation is its members': documented member by member, with this
///   function. [`type_too_large`] bounds how many members and how deep.
///
/// A frozen function (loaded from a file) keeps its full documentation, which `buck2 docs
/// starlark` and the LSP show too: only the file can give it large default values.
fn documentation<'v>(v: Value<'v>, cx: &RenderContext<'_, 'v>) -> Documentation {
    if !v.is_frozen() {
        match v.get_type() {
            "function" => {
                let typ = Ty::of_value(v);
                // Only a `def` (or a `lambda`) has the type of a function. The other values of
                // type `function` (bound methods such as `ctx.cquery`, partials, record types)
                // format no value in their documentation.
                if typ.as_function().is_some() {
                    let docs = function_name(v, &cx.budget)
                        .and_then(|name| cx.docstrings.get(&name))
                        .and_then(|raw| DocString::from_docstring(DocStringKind::Starlark, raw));
                    return Documentation::Own(Box::new(DocItem::Member(DocMember::Property(
                        DocProperty { docs, typ },
                    ))));
                }
            }
            "namespace" => {
                let members = v
                    .dir_attr()
                    .into_iter()
                    .filter_map(|name| {
                        let member = v.get_attr(&name, cx.heap).ok().flatten()?;
                        Some((name, documentation(member, cx).into_item()))
                    })
                    .collect();
                return Documentation::Own(Box::new(DocItem::Module(DocModule {
                    docs: None,
                    members,
                })));
            }
            _ => {}
        }
    }
    match v.documentation() {
        // A value of type `function` documented by its type (a record type, an enum type, ...)
        // is not an instance: its type is what it creates, not the `function` type.
        DocItem::Member(DocMember::Property(property)) if v.get_type() != "function" => {
            Documentation::Instance(property)
        }
        item => Documentation::Own(Box::new(item)),
    }
}

/// The name a function shows (`<repl:3>.f`: its `Display`), unless it is longer than
/// [`MAX_FUNCTION_NAME_BYTES`].
fn function_name(v: Value, budget: &RenderBudget<'_>) -> Option<String> {
    let mut out = RenderWriter {
        buf: CappedString::new(MAX_FUNCTION_NAME_BYTES),
        stack_exhausted: false,
        budget,
        writes: 0,
        stopped: None,
    };
    // The writer fails once the name is too long.
    fmt::write(&mut out, format_args!("{v}")).ok()?;
    Some(out.buf.into_string())
}

/// The documentation of the type of an instance, from the types of the globals.
fn type_documentation(v: Value, type_name: &str, cx: &RenderContext<'_, '_>) -> Option<DocItem> {
    let types = cx.types?;
    let ty = types.get(type_name).or_else(|| types.get(v.get_type()))?;
    Some(DocItem::Type(ty.clone()))
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
/// bytes, when the native stack runs low, and when the budget is spent.
///
/// Failing is safe: formatting code propagates `fmt::Error` with `?` (no `Display` impl in
/// buck2 or starlark-rust unwraps a write to its formatter), and `fmt::write` returns it.
struct RenderWriter<'a> {
    buf: CappedString,
    stack_exhausted: bool,
    budget: &'a RenderBudget<'a>,
    writes: u32,
    stopped: Option<Stop>,
}

impl Write for RenderWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if stacker::remaining_stack().is_some_and(|left| left < RENDER_STACK_RESERVE) {
            self.stack_exhausted = true;
            return Err(fmt::Error);
        }
        self.writes = self.writes.wrapping_add(1);
        if self.writes.is_multiple_of(BUDGET_CHECK_INTERVAL)
            && let Err(stop) = self.budget.check()
        {
            self.stopped = Some(stop);
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

/// The children of a container are fetched this many at a time, so that the check holds at most
/// one chunk per container on its path, however large the containers are.
const CHILDREN_CHUNK: usize = 1024;

/// Fetching the children of a struct or a record from the `n`th one on takes time in `n` (their
/// iterators cannot skip), so fetching all `n` of them in chunks of `c` takes time in
/// `n * n / 2c`. Their chunks double in size, up to this, rather than stay at
/// [`CHILDREN_CHUNK`]: the check of a struct with 3 million fields takes 70 million steps rather
/// than 4 billion, and holds at most 512 KiB of its fields.
const MAX_FIELDS_CHUNK: usize = 64 << 10;

/// Up to `max` of the values that formatting `v` recurses into, from the `from`th on, in the
/// order the formatter visits them; `None` if `v` is not a container.
fn children<'v>(v: Value<'v>, from: usize, max: usize) -> Option<Vec<Value<'v>>> {
    fn chunk<'v>(it: impl Iterator<Item = Value<'v>>, from: usize, max: usize) -> Vec<Value<'v>> {
        it.skip(from).take(max).collect()
    }
    // The iterators of lists, tuples and of the keys and the values of a dict skip in constant
    // time; the iterator of a dict's entries does not.
    if let Some(list) = ListRef::from_value(v) {
        Some(chunk(list.iter(), from, max))
    } else if let Some(tuple) = TupleRef::from_value(v) {
        Some(chunk(tuple.iter(), from, max))
    } else if let Some(dict) = DictRef::from_value(v) {
        let entry = from / 2;
        let entries = dict.keys().skip(entry).zip(dict.values().skip(entry));
        Some(chunk(entries.flat_map(<[Value; 2]>::from), from % 2, max))
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
    /// How many children the next fetch asks for.
    chunk: usize,
    /// The chunks grow (see [`MAX_FIELDS_CHUNK`]).
    growing: bool,
    /// Every child has been fetched.
    complete: bool,
}

impl<'v> Frame<'v> {
    /// The frame of `value`, with its first children; `None` if it is not a container.
    fn enter(value: Value<'v>) -> Option<Self> {
        let first = children(value, 0, CHILDREN_CHUNK)?;
        let mut frame = Frame {
            value,
            pending: Vec::new(),
            fetched: 0,
            chunk: CHILDREN_CHUNK,
            growing: StructRef::from_value(value).is_some() || Record::from_value(value).is_some(),
            complete: false,
        };
        frame.add(first);
        Some(frame)
    }

    fn add(&mut self, mut chunk: Vec<Value<'v>>) {
        self.complete = chunk.len() < self.chunk;
        self.fetched = self.fetched.saturating_add(chunk.len());
        if self.growing {
            self.chunk = self.chunk.saturating_mul(2).min(MAX_FIELDS_CHUNK);
        }
        chunk.reverse();
        self.pending = chunk;
    }

    fn next_child(&mut self) -> Option<Value<'v>> {
        if self.pending.is_empty() && !self.complete {
            let chunk = children(self.value, self.fetched, self.chunk).unwrap_or_default();
            self.add(chunk);
        }
        self.pending.pop()
    }
}

/// The number of bits of `v` if it is an int too large to format (more than
/// [`MAX_SHOWN_INT_BITS`]).
fn huge_int_bits(v: Value) -> Option<u64> {
    if v.unpack_i32().is_some() || v.get_type() != "int" {
        return None;
    }
    // A copy of the number (its size is not otherwise available): fast next to formatting it.
    let bits = BigInt::unpack_value(v).ok().flatten()?.bits();
    (bits > MAX_SHOWN_INT_BITS).then_some(bits)
}

/// What the depth check found.
struct Nesting {
    /// How deep formatting recurses into containers (`limit + 1` if deeper than `limit`).
    depth: usize,
    /// The number of bits of an int visited that is too large to format
    /// ([`MAX_SHOWN_INT_BITS`]): the check stops there.
    huge_int: Option<u64>,
}

/// How deep formatting `root` recurses into containers, checked without recursion, and whether
/// it would format an int too large to format. Fails when the budget is spent.
///
/// Walks the value the way the formatter does: depth first, children in order, and a value that
/// is already on the path is not entered again (the formatter shows it as `[...]`). The walk
/// stops where the formatter would have stopped at its size cap (`max_visits`: the formatter
/// writes at least one byte, an opening bracket, a separator or a key, before every value it
/// visits below the top one), so it costs time and memory bounded by the size of the rendering,
/// not by the size of the value (INV-9): the part of a value past the cap is never formatted, so
/// its depth does not matter.
///
/// Values the walk does not look into (sets, providers, ...) are left to the stack check of
/// [`RenderWriter`] (and the ints in them are formatted).
fn nesting_depth(
    root: Value,
    limit: usize,
    max_visits: usize,
    budget: &RenderBudget<'_>,
) -> Result<Nesting, Stop> {
    let Some(frame) = Frame::enter(root) else {
        return Ok(Nesting {
            depth: 0,
            huge_int: None,
        });
    };
    let mut on_path = HashSet::from([root.identity()]);
    let mut path = vec![frame];
    let mut deepest = 1;
    let mut visits = 0usize;
    while let Some(frame) = path.last_mut() {
        let Some(child) = frame.next_child() else {
            on_path.remove(&frame.value.identity());
            path.pop();
            continue;
        };
        visits += 1;
        if visits > max_visits {
            break;
        }
        if visits.is_multiple_of(CHILDREN_CHUNK * BUDGET_CHECK_INTERVAL as usize) {
            budget.check()?;
        }
        if let Some(bits) = huge_int_bits(child) {
            return Ok(Nesting {
                depth: deepest,
                huge_int: Some(bits),
            });
        }
        if on_path.contains(&child.identity()) {
            continue;
        }
        let Some(frame) = Frame::enter(child) else {
            continue;
        };
        if path.len() >= limit {
            return Ok(Nesting {
                depth: limit + 1,
                huge_int: None,
            });
        }
        on_path.insert(child.identity());
        path.push(frame);
        deepest = deepest.max(path.len());
    }
    Ok(Nesting {
        depth: deepest,
        huge_int: None,
    })
}
