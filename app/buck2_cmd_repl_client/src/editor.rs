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

use std::borrow::Cow;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use buck2_cli_proto::ReplDone;
use buck2_cli_proto::ReplEval;
use buck2_cli_proto::ReplHangup;
use buck2_cli_proto::ReplNotice;
use buck2_cli_proto::ReplReady;
use buck2_cli_proto::ReplRequest;
use buck2_cli_proto::repl_request;
use buck2_core::buck2_env;
use buck2_repl_syntax::commands::CommandId;
use buck2_repl_syntax::commands::Handler;
use buck2_repl_syntax::commands::parse_command;
use buck2_repl_syntax::completeness::Completeness;
use buck2_repl_syntax::completeness::completeness;
use dupe::Dupe;
use rustyline::Cmd;
use rustyline::Completer;
use rustyline::CompletionType;
use rustyline::ConditionalEventHandler;
use rustyline::Config;
use rustyline::Editor;
use rustyline::Event;
use rustyline::EventContext;
use rustyline::EventHandler;
use rustyline::Helper;
use rustyline::Hinter;
use rustyline::KeyCode;
use rustyline::KeyEvent;
use rustyline::Modifiers;
use rustyline::RepeatCount;
use rustyline::Validator;
use rustyline::error::ReadlineError;
use rustyline::highlight::CmdKind;
use rustyline::highlight::Highlighter;
use rustyline::highlight::MatchingBracketHighlighter;
use rustyline::hint::HistoryHinter;
use rustyline::history::FileHistory;
use rustyline::validate::ValidationContext;
use rustyline::validate::ValidationResult;

use crate::complete::Completer;
use crate::complete::ReplCompleter;
use crate::complete::complete_command;
use crate::help;
use crate::render;
use crate::render::Style;
use crate::run;
use crate::run::RunEnv;
use crate::session::InputOutcome;
use crate::session::OPEN_ID;
use crate::session::SharedUi;
use crate::session::ShutdownHangup;
use crate::session::UiEvent;
use crate::session::UiState;

/// Most entries kept in the history file.
const MAX_HISTORY: usize = 10_000;

/// What Tab inserts at the start of a line.
const INDENT: &str = "    ";

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

/// Enter submits the buffer only when it is complete.
struct ReplValidator;

impl rustyline::validate::Validator for ReplValidator {
    fn validate(&self, ctx: &mut ValidationContext<'_>) -> rustyline::Result<ValidationResult> {
        Ok(match completeness(ctx.input()) {
            Completeness::Complete => ValidationResult::Valid(None),
            Completeness::Incomplete(_) => ValidationResult::Incomplete,
        })
    }
}

#[derive(Helper, Completer, Hinter, Validator)]
struct ReplHelper {
    #[rustyline(Completer)]
    completer: ReplCompleter,
    #[rustyline(Hinter)]
    hinter: HistoryHinter,
    #[rustyline(Validator)]
    validator: ReplValidator,
    brackets: MatchingBracketHighlighter,
}

impl Highlighter for ReplHelper {
    fn highlight<'l>(&self, line: &'l str, pos: usize) -> Cow<'l, str> {
        self.brackets.highlight(line, pos)
    }

    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        default: bool,
    ) -> Cow<'b, str> {
        if default {
            Cow::Owned(format!("\x1b[1m{prompt}\x1b[0m"))
        } else {
            Cow::Borrowed(prompt)
        }
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(format!("\x1b[2m{hint}\x1b[0m"))
    }

    fn highlight_char(&self, line: &str, pos: usize, kind: CmdKind) -> bool {
        self.brackets.highlight_char(line, pos, kind)
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

/// How the inputs go.
enum Next {
    Continue,
    Stop,
}

pub(crate) struct EditorMode {
    pub(crate) req_tx: tokio::sync::mpsc::UnboundedSender<ReplRequest>,
    pub(crate) ui_rx: std::sync::mpsc::Receiver<UiEvent>,
    pub(crate) next_id: Arc<AtomicU64>,
    pub(crate) completer: Arc<Completer>,
    pub(crate) ui: SharedUi,
    pub(crate) history: Option<PathBuf>,
    pub(crate) outcome_tx: tokio::sync::oneshot::Sender<InputOutcome>,
    pub(crate) shutdown: ShutdownHangup,
    pub(crate) run_env: RunEnv,
}

impl EditorMode {
    /// Runs on the input thread. Reports the outcome, then hangs up.
    pub(crate) fn run(self) {
        let EditorMode {
            req_tx,
            ui_rx,
            next_id,
            completer,
            ui,
            history,
            outcome_tx,
            shutdown,
            run_env,
        } = self;
        let mut session = Session {
            req_tx,
            ui_rx,
            next_id,
            completer,
            ui,
            history,
            history_failed: false,
            shutdown,
            run_env,
            style: Style::interactive(),
            number: 0,
            outcome: InputOutcome::default(),
        };
        session.run();
        let _ignored = outcome_tx.send(std::mem::take(&mut session.outcome));
        let _ignored = session.req_tx.send(ReplRequest {
            id: session.next_id.fetch_add(1, Ordering::Relaxed),
            request: Some(repl_request::Request::Hangup(ReplHangup {})),
        });
    }
}

struct Session {
    req_tx: tokio::sync::mpsc::UnboundedSender<ReplRequest>,
    ui_rx: std::sync::mpsc::Receiver<UiEvent>,
    next_id: Arc<AtomicU64>,
    completer: Arc<Completer>,
    ui: SharedUi,
    history: Option<PathBuf>,
    /// The history file could not be written: it is not tried again.
    history_failed: bool,
    shutdown: ShutdownHangup,
    run_env: RunEnv,
    style: Style,
    /// Number of inputs sent so far; the daemon names input N `<repl:N>`.
    number: u32,
    outcome: InputOutcome,
}

impl Session {
    fn run(&mut self) {
        let Some(ready) = self.wait_for_ready() else {
            return;
        };
        let mut editor = match self.editor() {
            Ok(editor) => editor,
            Err(e) => {
                self.output(render::print_error(
                    self.style,
                    &format!("error: cannot start the line editor: {e}"),
                ));
                self.outcome.failed = true;
                return;
            }
        };
        let prompt = format!("{}> ", ready.cwd);

        // Consecutive Ctrl-Cs at the prompt.
        let mut interrupts: u32 = 0;
        loop {
            if !self.ui.start_reading() {
                // The session is over; the caller reports why.
                self.outcome.lost = true;
                return;
            }
            let line = editor.readline(&prompt);
            self.ui.stop_reading();
            let line = match line {
                Ok(line) => line,
                Err(ReadlineError::Interrupted) => {
                    interrupts = interrupts.saturating_add(1);
                    if interrupts >= 2
                        && !self
                            .output(render::print_note(self.style, "(use :q or Ctrl-D to exit)"))
                    {
                        return;
                    }
                    continue;
                }
                Err(ReadlineError::Eof) => return,
                Err(e) => {
                    self.output(render::print_error(
                        self.style,
                        &format!("error: cannot read input: {e}"),
                    ));
                    self.outcome.failed = true;
                    return;
                }
            };
            interrupts = 0;
            self.save_history(&mut editor);
            if line.trim().is_empty() {
                continue;
            }
            if let Next::Stop = self.eval(line) {
                return;
            }
        }
    }

    /// The line editor, with the history loaded.
    fn editor(&mut self) -> rustyline::Result<Editor<ReplHelper, FileHistory>> {
        let config = Config::builder()
            .completion_type(CompletionType::List)
            .completion_show_all_if_ambiguous(true)
            .bracketed_paste(true)
            .auto_add_history(true)
            .history_ignore_space(true)
            .max_history_size(MAX_HISTORY)?
            .build();
        let mut editor = Editor::with_config(config)?;
        editor.set_helper(Some(ReplHelper {
            completer: ReplCompleter(self.completer.dupe()),
            hinter: HistoryHinter::new(),
            validator: ReplValidator,
            brackets: MatchingBracketHighlighter::new(),
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
        if let Some(path) = &self.history {
            match editor.load_history(path) {
                Ok(()) => {}
                Err(ReadlineError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    self.output(render::print_note(
                        self.style,
                        &format!(
                            "warning: cannot read the history file `{}`: {e}",
                            path.display()
                        ),
                    ));
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
            self.output(render::print_note(
                self.style,
                &format!("warning: cannot write the history file `{path}`: {e}"),
            ));
        }
    }

    /// Waits for the answer to `Open` and prints the banner. `None` if the session could not
    /// start.
    fn wait_for_ready(&mut self) -> Option<ReplReady> {
        // Notices sent while the session starts (e.g. that the prelude could not be loaded) are
        // printed after the banner.
        let mut notices = Vec::new();
        let ready = loop {
            match self.ui_rx.recv() {
                Ok(UiEvent::Ready(ready)) => break ready,
                Ok(UiEvent::Notice(notice)) => notices.push(notice),
                Ok(UiEvent::Done(id, done)) if id == OPEN_ID => {
                    // The daemon could not start the session.
                    self.print_notices(&notices);
                    let rendered = render::render_done(&done, self.style).map(|_| ());
                    self.output(rendered);
                    self.outcome.failed = true;
                    return None;
                }
                Ok(UiEvent::Done(..)) => {}
                Ok(UiEvent::SessionEnded) | Err(_) => {
                    self.outcome.lost = true;
                    return None;
                }
            }
        };
        let banner = format!(
            "buck2 repl · {} · ctx is a bxl.Context · :help for commands · Ctrl-D to exit",
            ready.cwd
        );
        let printed = buck2_client_ctx::println!("{}", banner).and_then(|()| render::flush());
        if !self.output(printed) {
            return None;
        }
        self.print_notices(&notices);
        Some(ready)
    }

    fn print_notices(&mut self, notices: &[ReplNotice]) {
        for notice in notices {
            self.output(render::print_notice(self.style, notice));
        }
    }

    /// Evaluates one input and renders its result.
    fn eval(&mut self, input: String) -> Next {
        match parse_command(&input) {
            Ok(Some(command)) => match command.spec.id {
                CommandId::Quit => return Next::Stop,
                CommandId::Help => {
                    let printed = match help::help(&command.arg) {
                        Ok(text) => render::print_text(&text).and_then(|()| render::flush()),
                        Err(message) => {
                            render::print_error(self.style, &format!("error: {message}"))
                        }
                    };
                    return self.continue_if(printed);
                }
                CommandId::Time => {
                    let timed = command.arg.into_owned();
                    return self.eval_timed(timed);
                }
                CommandId::Complete => {
                    let printed = match complete_command(&self.completer, &command.arg) {
                        Ok(json) => render::print_text(&json).and_then(|()| render::flush()),
                        Err(message) => {
                            render::print_error(self.style, &format!("error: {message}"))
                        }
                    };
                    return self.continue_if(printed);
                }
                _ => {}
            },
            Ok(None) => {}
            Err(e) => {
                return self.continue_if(render::print_error(self.style, &format!("error: {e}")));
            }
        }
        match self.request(input) {
            Some(done) => self.finish(&done, self.style, None),
            None => Next::Stop,
        }
    }

    /// `:time <input>`: evaluates the input, then prints how long it took.
    fn eval_timed(&mut self, mut input: String) -> Next {
        loop {
            match parse_command(&input) {
                // `:time :time x` times `x`.
                Ok(Some(command)) if command.spec.id == CommandId::Time => {
                    let timed = command.arg.into_owned();
                    input = timed;
                }
                // Nothing to time: handled here.
                Ok(Some(command)) if command.spec.handler == Handler::Client => {
                    return self.eval(input);
                }
                Err(_) => return self.eval(input),
                Ok(_) => break,
            }
        }
        let start = Instant::now();
        match self.request(input) {
            Some(done) => {
                let total = Instant::now() - start;
                self.finish(&done, self.style.without_durations(), Some(total))
            }
            None => Next::Stop,
        }
    }

    /// Sends an input to the daemon and waits for its result. `None` if the session ended.
    fn request(&mut self, input: String) -> Option<ReplDone> {
        self.number = self.number.saturating_add(1);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = ReplRequest {
            id,
            request: Some(repl_request::Request::Eval(ReplEval {
                input,
                number: self.number,
            })),
        };
        self.ui.set(UiState::Busy { id, presses: 0 });
        if self.req_tx.send(request).is_err() {
            self.outcome.lost = true;
            return None;
        }
        let done = self.wait_for_done(id);
        self.ui.set(UiState::Editor);
        if done.is_none() {
            self.outcome.lost = true;
        }
        done
    }

    /// Renders the result of an input (and how long it took, for `:time`).
    fn finish(&mut self, done: &ReplDone, style: Style, timing: Option<Duration>) -> Next {
        if self.outcome.output_error.is_some() {
            // A notice could not be printed.
            return Next::Stop;
        }
        let rendered = render::render_done(done, style).and_then(|r| {
            let r = match r {
                render::Rendered::Run(run) => {
                    run::run_program(&run, &self.run_env, &self.ui, UiState::Editor, style)?
                }
                r => r,
            };
            if let Some(total) = timing {
                render::print_timing(style, total, done)?;
            }
            render::flush().map(|()| r)
        });
        match rendered {
            Ok(render::Rendered::Ok | render::Rendered::Run(_)) => Next::Continue,
            Ok(render::Rendered::Failed | render::Rendered::Interrupted) => {
                if self.shutdown.is_shutting_down() {
                    // The daemon cancels the input in flight when it shuts down.
                    self.outcome.after_shutdown = true;
                    Next::Stop
                } else {
                    Next::Continue
                }
            }
            Err(e) => self.continue_if(Err(e)),
        }
    }

    /// Records the first output error. Returns whether the output was written.
    fn output(&mut self, result: buck2_error::Result<()>) -> bool {
        match result {
            Ok(()) => true,
            Err(e) => {
                if self.outcome.output_error.is_none() {
                    self.outcome.output_error = Some(e);
                }
                false
            }
        }
    }

    /// Goes on if the output was written.
    fn continue_if(&mut self, result: buck2_error::Result<()>) -> Next {
        if self.output(result) {
            Next::Continue
        } else {
            Next::Stop
        }
    }

    /// Waits for the result of request `id`, printing notices meanwhile (an output error is
    /// recorded). `None` if the session ended first.
    fn wait_for_done(&mut self, id: u64) -> Option<ReplDone> {
        loop {
            match self.ui_rx.recv() {
                Ok(UiEvent::Done(done_id, done)) if done_id == id => return Some(done),
                Ok(UiEvent::Notice(notice)) => {
                    let printed = render::print_notice(self.style, &notice);
                    self.output(printed);
                }
                Ok(UiEvent::Done(..) | UiEvent::Ready(_)) => {}
                Ok(UiEvent::SessionEnded) | Err(_) => return None,
            }
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
