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

use buck2_core::buck2_env;
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

use crate::complete::ReplCompleter;
use crate::inputs::FirstInputs;
use crate::inputs::Inputs;
use crate::inputs::Next;
use crate::render;
use crate::session::InputOutcome;
use crate::session::SharedUi;

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
        let printed = buck2_client_ctx::println!("{}", banner).and_then(|()| render::flush());
        if !self.inputs.output(printed) {
            return;
        }
        self.inputs.print_notices(&notices);
        let mut editor = match self.editor() {
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
        let prompt = format!("{}> ", ready.cwd);

        // Consecutive Ctrl-Cs at the prompt.
        let mut interrupts: u32 = 0;
        loop {
            if !self.inputs.ui.start_reading() {
                // The session is over; the caller reports why.
                self.inputs.outcome.lost = true;
                return;
            }
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
            self.save_history(&mut editor);
            if line.trim().is_empty() {
                continue;
            }
            if let Next::Stop = self.inputs.eval(line) {
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
            completer: ReplCompleter::new(self.inputs.completer.dupe()),
            hinter: HistoryHinter::new(),
            validator: ReplValidator(self.inputs.ui.dupe()),
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
