/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Interactive mode: a line editor reads the inputs.
//!
//! Enter submits a complete input and adds a line to an incomplete one (the rules of
//! [`completeness`]); Alt-Enter always adds a line; Tab indents at the start of a line. Ctrl-C
//! clears the line, Ctrl-D on an empty line ends the session. The history is kept in a file.
//!
//! The input is highlighted as it is typed ([`highlight`]: keywords, strings, numbers, comments,
//! the command, the bracket matching the one at the cursor), unless colour is off (`NO_COLOR`,
//! stdout not a terminal, `:set color off`). In the parentheses of a call, the signature of the
//! function is shown under the input when completion has offered that function since the last
//! input (the daemon sends the signatures with the candidates: no request is made per key).

use std::borrow::Cow;
use std::cell::Cell;
use std::cell::RefCell;
use std::io::Write;
use std::ops::Range;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use buck2_core::buck2_env;
use buck2_repl_syntax::completeness::Completeness;
use buck2_repl_syntax::completeness::completeness;
use buck2_repl_syntax::highlight;
use buck2_repl_syntax::highlight::Class;
use buck2_repl_syntax::highlight::Span;
use buck2_repl_syntax::signature::active_parameter;
use buck2_repl_syntax::site::Step;
use buck2_repl_syntax::site::chain_key;
use buck2_repl_syntax::site::enclosing_call;
use buck2_repl_syntax::terminal::Position;
use buck2_repl_syntax::terminal::editor_position;
use dupe::Dupe;
use rustyline::Cmd;
use rustyline::ColorMode;
use rustyline::Completer;
use rustyline::CompletionType;
use rustyline::ConditionalEventHandler;
use rustyline::Config;
use rustyline::Context;
use rustyline::Editor;
use rustyline::Event;
use rustyline::EventContext;
use rustyline::EventHandler;
use rustyline::GraphemeClusterMode;
use rustyline::Helper;
use rustyline::Hinter;
use rustyline::KeyCode;
use rustyline::KeyEvent;
use rustyline::Modifiers;
use rustyline::RepeatCount;
use rustyline::Validator;
use rustyline::completion::Pair;
use rustyline::completion::longest_common_prefix;
use rustyline::config::Configurer;
use rustyline::error::ReadlineError;
use rustyline::highlight::CmdKind;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hint;
use rustyline::hint::HistoryHinter;
use rustyline::history::FileHistory;
use rustyline::line_buffer::LineBuffer;
use rustyline::validate::ValidationContext;
use rustyline::validate::ValidationResult;

use crate::complete::Completer as DaemonCompleter;
use crate::complete::ReplCompleter;
use crate::inputs::FirstInputs;
use crate::inputs::Inputs;
use crate::inputs::Next;
use crate::render;
use crate::session::InputOutcome;
use crate::session::SharedUi;
use crate::settings::Switch;

/// Most entries kept in the history file.
const MAX_HISTORY: usize = 10_000;

/// What Tab inserts at the start of a line.
const INDENT: &str = "    ";

/// An input longer than this (in bytes) is not shown again when typing at its end only makes
/// its last highlighted part longer (see `ReplHelper::highlight_char`).
const LONG_INPUT: usize = 512;

/// Where the history is kept: `BUCK2_REPL_HISTORY` (empty for none), otherwise
/// `~/.buck/repl_history`. `None` with `--no-history`.
pub(crate) fn history_path(no_history: bool) -> buck2_error::Result<Option<PathBuf>> {
    if no_history {
        return Ok(None);
    }
    Ok(match buck2_env!("BUCK2_REPL_HISTORY")? {
        Some("") => None,
        Some(path) => Some(PathBuf::from(path)),
        None => dirs::home_dir().map(|home| home.join(".buck").join("repl_history")),
    })
}

/// Enter submits the buffer only when it is complete, or when the session is over (so that
/// Enter ends the line, as the message about the end of the session says).
struct ReplValidator(SharedUi);

impl rustyline::validate::Validator for ReplValidator {
    fn validate(&self, ctx: &mut ValidationContext<'_>) -> rustyline::Result<ValidationResult> {
        if self.0.session_ended() {
            return Ok(ValidationResult::Valid(None));
        }
        Ok(match completeness(ctx.input()) {
            Completeness::Complete => ValidationResult::Valid(None),
            Completeness::Incomplete(_) => ValidationResult::Incomplete,
        })
    }
}

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";

/// What is shown after the input.
enum ReplHint {
    /// The rest of an earlier input that the input starts (fish-style): → or End inserts it.
    History(String),
    /// The signature of the function whose call the cursor is in, on a line of its own under
    /// the input (`\n` + `name(params) -> type`): nothing to insert.
    Signature(String),
}

impl Hint for ReplHint {
    fn display(&self) -> &str {
        match self {
            ReplHint::History(text) | ReplHint::Signature(text) => text,
        }
    }

    fn completion(&self) -> Option<&str> {
        match self {
            ReplHint::History(text) => Some(text),
            ReplHint::Signature(_) => None,
        }
    }
}

/// History hints, else signature hints.
struct ReplHinter {
    history: HistoryHinter,
    completer: Arc<DaemonCompleter>,
    /// The last signature hint, and the range of its parameter that the argument at the cursor
    /// fills, which is shown in bold (the rest is faint).
    last_signature: RefCell<Option<(String, Option<Range<usize>>)>>,
    listing: Rc<Listing>,
}

impl rustyline::hint::Hinter for ReplHinter {
    type Hint = ReplHint;

    fn hint(&self, line: &str, pos: usize, ctx: &Context<'_>) -> Option<ReplHint> {
        if self.listing.hides_hint(line, pos) {
            return None;
        }
        if let Some(rest) = self.history.hint(line, pos, ctx) {
            return Some(ReplHint::History(rest));
        }
        let call = enclosing_call(line, pos)?;
        let name = match call.steps.last() {
            None => call.root,
            Some(Step::Attr(name)) => *name,
            // `f()(`: completion offers no such function.
            Some(Step::Call) => return None,
        };
        let signature = self
            .completer
            .signature(&chain_key(call.root, &call.steps))?;
        let display = format!("\n{name}{signature}");
        // The signature starts after the newline and the name.
        let shift = 1 + name.len();
        let active = active_parameter(&signature, call.positional, call.keyword)
            .map(|r| r.start + shift..r.end + shift);
        *self.last_signature.borrow_mut() = Some((display.clone(), active));
        Some(ReplHint::Signature(display))
    }
}

impl ReplHinter {
    /// `hint` styled for the terminal: faint, with the parameter at the cursor in bold if it is
    /// the last signature hint.
    fn styled(&self, hint: &str) -> String {
        let last = self.last_signature.borrow();
        let active = match &*last {
            Some((display, active)) if display == hint => active.clone(),
            _ => None,
        };
        let (newline, text, active) = match hint.strip_prefix('\n') {
            Some(text) => (
                "\n",
                text,
                active.and_then(|r| Some(r.start.checked_sub(1)?..r.end.checked_sub(1)?)),
            ),
            None => ("", hint, active),
        };
        let parts = active.and_then(|r| {
            Some((
                text.get(..r.start)?,
                text.get(r.clone())?,
                text.get(r.end..)?,
            ))
        });
        match parts {
            Some((before, active, after)) => format!(
                "{newline}{DIM}{before}{RESET}{BOLD}{active}{RESET}{DIM}{after}{RESET}",
                RESET = highlight::RESET
            ),
            None => format!("{newline}{DIM}{text}{RESET}", RESET = highlight::RESET),
        }
    }
}

/// What the syntax highlighter has shown.
#[derive(Default)]
struct SyntaxState {
    /// Whether the bracket matching the one at the cursor is shown: not when the input is
    /// shown for the last time (it stays on the screen).
    brackets: Cell<bool>,
    /// The highlighted parts of the input as last shown.
    shown: RefCell<Vec<Span>>,
}

#[derive(Helper, Completer, Hinter, Validator)]
struct ReplHelper {
    #[rustyline(Completer)]
    completer: ListingCompleter,
    #[rustyline(Hinter)]
    hinter: ReplHinter,
    #[rustyline(Validator)]
    validator: ReplValidator,
    syntax: SyntaxState,
}

/// rustyline calls the highlighter only when colour is on (see [`color_mode`]).
impl Highlighter for ReplHelper {
    fn highlight<'l>(&self, line: &'l str, pos: usize) -> Cow<'l, str> {
        let spans = highlight::spans(line, self.syntax.brackets.get().then_some(pos));
        let painted = if spans.is_empty() {
            Cow::Borrowed(line)
        } else {
            Cow::Owned(highlight::paint(line, &spans, highlight::ansi_style))
        };
        *self.syntax.shown.borrow_mut() = spans;
        painted
    }

    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        default: bool,
    ) -> Cow<'b, str> {
        if default {
            Cow::Owned(format!("{BOLD}{prompt}{}", highlight::RESET))
        } else {
            Cow::Borrowed(prompt)
        }
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(self.hinter.styled(hint))
    }

    /// Whether the input must be shown again after an edit or a move of the cursor (the input
    /// is `line` now, the cursor at `pos`): when what is highlighted changes, or a matching
    /// bracket is shown (so that it is taken away when the input is shown for the last time).
    /// Otherwise rustyline writes a character typed (or erases one) at the end as it is, which
    /// is right only if it is not highlighted and nothing else changes.
    ///
    /// Typing at the end of a long input inside a string or a comment only makes its last part
    /// longer: that is not shown again (the characters show unstyled until the next repaint), as
    /// repainting the whole input for each character would write output that grows with the
    /// square of its length (a paste into a terminal without bracketed paste).
    fn highlight_char(&self, line: &str, pos: usize, kind: CmdKind) -> bool {
        let brackets = kind != CmdKind::ForcedRefresh;
        self.syntax.brackets.set(brackets);
        let spans = highlight::spans(line, brackets.then_some(pos));
        if spans.iter().any(|s| s.class == Class::MatchingBracket) {
            return true;
        }
        let shown = self.syntax.shown.borrow();
        if *shown == spans {
            return false;
        }
        let growing = kind == CmdKind::Other
            && line.len() > LONG_INPUT
            && pos == line.len()
            && highlight::only_last_span_grew(&shown, &spans, pos);
        !growing
    }
}

/// The colour mode of the line editor for `:set color`: by default, colour when stdout is a
/// terminal and `NO_COLOR` is not set (rustyline's `Enabled`).
fn color_mode(color: Switch) -> ColorMode {
    match color {
        Switch::Auto => ColorMode::Enabled,
        Switch::On => ColorMode::Forced,
        Switch::Off => ColorMode::Disabled,
    }
}

/// Tab indents when only whitespace is before the cursor on its line; otherwise it completes.
struct TabIndent;

impl ConditionalEventHandler for TabIndent {
    fn handle(
        &self,
        _evt: &Event,
        n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        let before = ctx.line().get(..ctx.pos())?;
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        if before.get(line_start..)?.chars().all(char::is_whitespace) {
            Some(Cmd::Insert(n, INDENT.to_owned()))
        } else {
            None
        }
    }
}

/// What Tab does to the screen when rustyline lists several candidates (or asks whether to,
/// `Display all N possibilities? (y or n)`): it moves the cursor to the end of the line of the
/// cursor and writes the list on the rows under it, over what they show, without erasing it.
/// Those rows hold the signature hint (drawn under the input) and the lines of the input after
/// the cursor's, which would be left between the candidates. So before the list is drawn,
/// [`ListingCompleter`] erases them, and when rustyline first inserts the candidates' common
/// prefix (it then draws the input again, hint included, just before the list), the
/// signature hint is hidden until the input changes or the cursor moves.
#[derive(Default)]
struct Listing {
    /// The input and the cursor for which the hint is hidden.
    hide_hint_at: RefCell<Option<(String, usize)>>,
}

impl Listing {
    /// Whether the hint of the input `line` with the cursor at `pos` is hidden: while the input
    /// is the one for which it was hidden (it is shown again once it changes).
    fn hides_hint(&self, line: &str, pos: usize) -> bool {
        let mut hidden = self.hide_hint_at.borrow_mut();
        match &*hidden {
            Some((l, p)) if l == line && *p == pos => true,
            Some(_) => {
                *hidden = None;
                false
            }
            None => false,
        }
    }
}

/// The completer of the line editor ([`ReplCompleter`]), which also clears the rows under the
/// input when rustyline is about to list the candidates there (see [`Listing`]).
struct ListingCompleter {
    inner: ReplCompleter,
    listing: Rc<Listing>,
    /// The prompt, which the input follows on the screen.
    prompt: String,
    /// How the editor measures text (its configuration).
    grapheme_mode: GraphemeClusterMode,
    tab_stop: usize,
}

impl rustyline::completion::Completer for ListingCompleter {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let (start, candidates) = self.inner.complete(line, pos, ctx)?;
        // One candidate is inserted; several are listed (`completion_show_all_if_ambiguous`).
        if candidates.len() > 1 {
            if let Some(prefix) = longest_common_prefix(&candidates)
                && prefix.len() > pos.saturating_sub(start)
                && let (Some(before), Some(after)) = (line.get(..start), line.get(pos..))
            {
                // rustyline inserts the common prefix (`update`) and draws the input again.
                *self.listing.hide_hint_at.borrow_mut() =
                    Some((format!("{before}{prefix}{after}"), start + prefix.len()));
            }
            self.erase_under_line(line, pos);
        }
        Ok((start, candidates))
    }

    fn update(
        &self,
        line: &mut LineBuffer,
        start: usize,
        elected: &str,
        cl: &mut rustyline::Changeset,
    ) {
        self.inner.update(line, start, elected, cl)
    }
}

impl ListingCompleter {
    /// Erases the rows under the end of the line of the cursor (of the input `line`, the cursor
    /// at `pos`), to the end of the screen, and puts the cursor back. Where the input is drawn is
    /// computed as the editor computes it: after the prompt, wrapped at the terminal's width.
    fn erase_under_line(&self, line: &str, pos: usize) {
        let Some(columns) = render::terminal_columns() else {
            return;
        };
        let (Some(before), Some(after)) = (line.get(..pos), line.get(pos..)) else {
            return;
        };
        let line_end = pos + after.find('\n').unwrap_or(after.len());
        let Some(to_line_end) = line.get(..line_end) else {
            return;
        };
        let mode = self.grapheme_mode;
        let width = |g: &str| usize::from(mode.width(g));
        let at = |text: &str, start: Position| {
            editor_position(text, start, columns, self.tab_stop, width)
        };
        let input_start = at(&self.prompt, Position::default());
        let cursor = at(before, input_start);
        let end = at(to_line_end, input_start);
        // Save the cursor, go down to the row of the end of the line and to the start of the
        // next row (the terminal is in raw mode: `\n` alone does not return the carriage), erase
        // from there to the end of the screen, put the cursor back. The input is drawn above
        // the hint, so there is always a row under the end of its line: `\n` does not scroll.
        let down = end.row.saturating_sub(cursor.row);
        let mut seq = String::from("\x1b7");
        if down > 0 {
            seq.push_str(&format!("\x1b[{down}B"));
        }
        seq.push_str("\r\n\x1b[J\x1b8");
        let mut stdout = std::io::stdout().lock();
        let _ignored = stdout
            .write_all(seq.as_bytes())
            .and_then(|()| stdout.flush());
    }
}

/// End and Ctrl-E insert the history hint shown after the input (fish-style), as → does
/// (rustyline binds only →), when the cursor is at the end of the input; otherwise they move
/// the cursor as usual.
struct AcceptHistoryHint;

impl ConditionalEventHandler for AcceptHistoryHint {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        // Only a history hint inserts text (rustyline gives out only that text).
        let history = ctx.hint_text().is_some_and(|hint| !hint.is_empty());
        (history && ctx.pos() == ctx.line().len()).then_some(Cmd::CompleteHint)
    }
}

pub(crate) struct EditorMode {
    pub(crate) inputs: Inputs,
    /// The files and `-e` inputs of the command line, evaluated before the prompt.
    pub(crate) first: FirstInputs,
    pub(crate) history: Option<PathBuf>,
    pub(crate) outcome_tx: tokio::sync::oneshot::Sender<InputOutcome>,
}

impl EditorMode {
    /// Runs on the input thread. Reports the outcome, then hangs up.
    pub(crate) fn run(self) {
        let EditorMode {
            inputs,
            first,
            history,
            outcome_tx,
        } = self;
        let mut session = Session {
            inputs,
            history,
            history_failed: false,
        };
        session.run(first);
        session.inputs.finish_session(outcome_tx);
    }
}

struct Session {
    inputs: Inputs,
    history: Option<PathBuf>,
    /// The history file could not be written: it is not tried again.
    history_failed: bool,
}

impl Session {
    fn run(&mut self, first: FirstInputs) {
        let Some((ready, notices)) = self.inputs.wait_for_ready() else {
            return;
        };
        let banner = format!(
            "buck2 repl · {} · ctx is a bxl.Context · :help for commands · Ctrl-D to exit",
            ready.cwd
        );
        let printed = render::print_text(&banner).and_then(|()| render::flush());
        if !self.inputs.output(printed) {
            return;
        }
        self.inputs.print_notices(&notices);
        let prompt = format!("{}> ", ready.cwd);
        let mut editor = match self.editor(&prompt) {
            Ok(editor) => editor,
            Err(e) => {
                let printed = render::print_error(
                    self.inputs.style(),
                    &format!("error: cannot start the line editor: {e}"),
                );
                self.inputs.output(printed);
                self.inputs.outcome.failed = true;
                return;
            }
        };
        if let Next::Stop = self.inputs.run_first(first) {
            return;
        }

        // Consecutive Ctrl-Cs at the prompt.
        let mut interrupts: u32 = 0;
        // Input that is not UTF-8 was ignored since the last line read.
        let mut not_utf8 = false;
        loop {
            if !self.inputs.ui.start_reading() {
                // The session is over; the caller reports why.
                self.inputs.outcome.lost = true;
                return;
            }
            editor.set_color_mode(color_mode(self.inputs.color_setting()));
            let line = editor.readline(&prompt);
            self.inputs.ui.stop_reading();
            if self.inputs.ui.session_ended() {
                // The line was ended to exit; the caller reports why.
                self.inputs.outcome.lost = true;
                return;
            }
            let line = match line {
                Ok(line) => line,
                Err(ReadlineError::Interrupted) => {
                    interrupts = interrupts.saturating_add(1);
                    if interrupts >= 2 {
                        let printed =
                            render::print_note(self.inputs.style(), "(use :q or Ctrl-D to exit)");
                        if !self.inputs.output(printed) {
                            return;
                        }
                    }
                    continue;
                }
                Err(ReadlineError::Eof) => return,
                // A byte that is not UTF-8 (an 8-bit Meta key, a paste of Latin-1 text): the
                // line so far is lost, the session is not (the error needs a byte of input, so
                // this cannot spin).
                Err(ReadlineError::Io(e)) if e.kind() == std::io::ErrorKind::InvalidData => {
                    if !not_utf8 {
                        not_utf8 = true;
                        let printed = render::print_note(
                            self.inputs.style(),
                            "warning: ignored input that is not UTF-8 (and the line typed)",
                        );
                        if !self.inputs.output(printed) {
                            return;
                        }
                    }
                    continue;
                }
                Err(e) => {
                    let printed = render::print_error(
                        self.inputs.style(),
                        &format!("error: cannot read input: {e}"),
                    );
                    self.inputs.output(printed);
                    self.inputs.outcome.failed = true;
                    return;
                }
            };
            interrupts = 0;
            not_utf8 = false;
            self.save_history(&mut editor);
            if line.trim().is_empty() {
                continue;
            }
            if let Next::Stop = self.inputs.eval(line) {
                return;
            }
        }
    }

    /// The line editor, with the history loaded. `prompt` is the prompt it shows.
    fn editor(&mut self, prompt: &str) -> rustyline::Result<Editor<ReplHelper, FileHistory>> {
        let config = Config::builder()
            .completion_type(CompletionType::List)
            .completion_show_all_if_ambiguous(true)
            .bracketed_paste(true)
            .auto_add_history(true)
            .max_history_size(MAX_HISTORY)?
            .build();
        let listing = Rc::new(Listing::default());
        let completer = ListingCompleter {
            inner: ReplCompleter::new(self.inputs.completer.dupe()),
            listing: listing.dupe(),
            prompt: prompt.to_owned(),
            grapheme_mode: config.grapheme_cluster_mode(),
            tab_stop: usize::from(config.tab_stop()),
        };
        let mut editor = Editor::with_config(config)?;
        editor.set_helper(Some(ReplHelper {
            completer,
            hinter: ReplHinter {
                history: HistoryHinter::new(),
                completer: self.inputs.completer.dupe(),
                last_signature: RefCell::new(None),
                listing,
            },
            validator: ReplValidator(self.inputs.ui.dupe()),
            syntax: SyntaxState::default(),
        }));
        // Alt-Enter (or Esc then Enter) adds a line even to a complete input.
        editor.bind_sequence(
            KeyEvent(KeyCode::Enter, Modifiers::ALT),
            EventHandler::Simple(Cmd::Newline),
        );
        editor.bind_sequence(
            KeyEvent(KeyCode::Tab, Modifiers::NONE),
            EventHandler::Conditional(Box::new(TabIndent)),
        );
        for key in [KeyEvent(KeyCode::End, Modifiers::NONE), KeyEvent::ctrl('E')] {
            editor.bind_sequence(key, EventHandler::Conditional(Box::new(AcceptHistoryHint)));
        }
        if let Some(path) = &self.history {
            match editor.load_history(path) {
                Ok(()) => {}
                Err(ReadlineError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    let printed = render::print_note(
                        self.inputs.style(),
                        &format!(
                            "warning: cannot read the history file `{}`: {e}",
                            path.display()
                        ),
                    );
                    self.inputs.output(printed);
                }
            }
        }
        Ok(editor)
    }

    /// Appends the new history entries to the history file.
    fn save_history(&mut self, editor: &mut Editor<ReplHelper, FileHistory>) {
        let Some(path) = &self.history else {
            return;
        };
        if self.history_failed {
            return;
        }
        if let Err(e) = append_history(editor, path) {
            self.history_failed = true;
            let path = path.display().to_string();
            let printed = render::print_note(
                self.inputs.style(),
                &format!("warning: cannot write the history file `{path}`: {e}"),
            );
            self.inputs.output(printed);
        }
    }
}

fn append_history(
    editor: &mut Editor<ReplHelper, FileHistory>,
    path: &Path,
) -> rustyline::Result<()> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    editor.append_history(path)
}
