/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! The inputs of a session, read by the interactive [`editor`](crate::editor) or by the
//! [`script`](crate::script) reader: the commands of the client (`:help`, `:time`, `:hist`,
//! `:!`, `:edit`, the client's part of `:set`, ...) are handled here, everything else is sent
//! to the daemon, and its result rendered. The two modes differ in how results are shown and in
//! what a failing input does ([`Mode`]).

use std::collections::VecDeque;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use buck2_cli_proto::ReplDone;
use buck2_cli_proto::ReplEval;
use buck2_cli_proto::ReplHangup;
use buck2_cli_proto::ReplNotice;
use buck2_cli_proto::ReplReady;
use buck2_cli_proto::ReplRequest;
use buck2_cli_proto::repl_done;
use buck2_cli_proto::repl_request;
use buck2_repl_syntax::commands::CommandId;
use buck2_repl_syntax::commands::Handler;
use buck2_repl_syntax::commands::SettingSide;
use buck2_repl_syntax::commands::command;
use buck2_repl_syntax::commands::parse_command;
use buck2_repl_syntax::commands::parse_count;
use buck2_repl_syntax::commands::parse_set_args;
use buck2_repl_syntax::commands::split_args;
use buck2_repl_syntax::commands::split_command_token;
use buck2_repl_syntax::text::truncate_to_bytes;

use crate::complete::Completer;
use crate::complete::complete_command;
use crate::help;
use crate::render;
use crate::render::Rendered;
use crate::render::Style;
use crate::run;
use crate::run::RunEnv;
use crate::session::InputOutcome;
use crate::session::OPEN_ID;
use crate::session::SharedUi;
use crate::session::ShutdownHangup;
use crate::session::UiEvent;
use crate::session::UiState;
use crate::settings::Settings;
use crate::settings::Switch;

/// Most inputs `:hist` keeps.
const MAX_HISTORY: usize = 1000;

/// Longest input `:hist` keeps (the start of a longer one).
const MAX_HISTORY_INPUT: usize = 16 << 10;

/// How the session reads its inputs.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Mode {
    /// The line editor: results are shown for people, and a failing input does not end the
    /// session.
    Interactive,
    /// Inputs from `-e` or stdin: a failing input ends the session unless `continue_on_error`.
    Script { continue_on_error: bool },
}

/// Whether the session goes on after an input.
pub(crate) enum Next {
    Continue,
    Stop,
}

/// What the session does before it reads inputs (`buck2 repl [FILES] [-e INPUT]...`).
#[derive(Debug, Default)]
pub(crate) struct FirstInputs {
    /// Files to load (`.bzl`, `.bxl`, as `:load` does) or to evaluate (anything else).
    pub(crate) files: Vec<String>,
    /// The `-e` inputs.
    pub(crate) evals: Vec<String>,
}

/// The inputs of the session so far, numbered like the daemon names them (`<repl:N>`), for
/// `:hist`.
#[derive(Default)]
struct InputHistory {
    inputs: VecDeque<(u32, String)>,
}

impl InputHistory {
    fn push(&mut self, number: u32, input: &str) {
        if self.inputs.len() >= MAX_HISTORY {
            self.inputs.pop_front();
        }
        let mut kept = truncate_to_bytes(input.trim_end(), MAX_HISTORY_INPUT).to_owned();
        if kept.len() < input.trim_end().len() {
            kept.push('…');
        }
        self.inputs.push_back((number, kept));
    }

    /// The last `count` inputs (all if `None`), one per paragraph: the number, then the lines
    /// of the input.
    fn render(&self, count: Option<usize>) -> String {
        if self.inputs.is_empty() {
            return "(no inputs yet)".to_owned();
        }
        let skip = count.map_or(0, |count| self.inputs.len().saturating_sub(count));
        let mut out = String::new();
        for (number, input) in self.inputs.iter().skip(skip) {
            for (i, line) in input.lines().enumerate() {
                if !out.is_empty() {
                    out.push('\n');
                }
                if i == 0 {
                    out.push_str(&format!("{number:>4}  {line}"));
                } else {
                    out.push_str(&format!("      {line}"));
                }
            }
        }
        out
    }
}

/// Where the editor of `:edit` opens, and what happens after.
struct EditTarget {
    path: PathBuf,
    line: Option<usize>,
    /// A module (`.bzl`, `.bxl`): the session loads it again if it is loaded.
    module: bool,
}

/// How the input thread talks to the session.
pub(crate) struct SessionIo {
    pub(crate) req_tx: tokio::sync::mpsc::UnboundedSender<ReplRequest>,
    pub(crate) ui_rx: std::sync::mpsc::Receiver<UiEvent>,
    pub(crate) next_id: Arc<AtomicU64>,
    pub(crate) completer: Arc<Completer>,
    pub(crate) ui: SharedUi,
    pub(crate) shutdown: ShutdownHangup,
    pub(crate) run_env: RunEnv,
}

/// Reads and evaluates inputs, on the input thread.
pub(crate) struct Inputs {
    req_tx: tokio::sync::mpsc::UnboundedSender<ReplRequest>,
    ui_rx: std::sync::mpsc::Receiver<UiEvent>,
    next_id: Arc<AtomicU64>,
    pub(crate) completer: Arc<Completer>,
    pub(crate) ui: SharedUi,
    shutdown: ShutdownHangup,
    run_env: RunEnv,
    mode: Mode,
    settings: Settings,
    /// Number of inputs sent so far; the daemon names input N `<repl:N>`.
    number: u32,
    history: InputHistory,
    /// The scratch buffer of `:edit`, as last edited.
    scratch: String,
    pub(crate) outcome: InputOutcome,
}

impl Inputs {
    pub(crate) fn new(io: SessionIo, mode: Mode) -> Self {
        let SessionIo {
            req_tx,
            ui_rx,
            next_id,
            completer,
            ui,
            shutdown,
            run_env,
        } = io;
        Inputs {
            req_tx,
            ui_rx,
            next_id,
            completer,
            ui,
            shutdown,
            run_env,
            mode,
            settings: Settings::default(),
            number: 0,
            history: InputHistory::default(),
            scratch: String::new(),
            outcome: InputOutcome::default(),
        }
    }

    fn interactive(&self) -> bool {
        matches!(self.mode, Mode::Interactive)
    }

    /// How results are shown now.
    pub(crate) fn style(&self) -> Style {
        let interactive = self.interactive();
        let color = match self.settings.color {
            Switch::Auto => interactive && Style::default_color(),
            Switch::On => true,
            Switch::Off => false,
        };
        Style::new(
            interactive,
            color,
            interactive && self.settings.timing == Switch::Auto,
        )
    }

    /// `:set color`.
    pub(crate) fn color_setting(&self) -> Switch {
        self.settings.color
    }

    /// The state of the terminal between inputs.
    fn idle_state(&self) -> UiState {
        match self.mode {
            Mode::Interactive => UiState::Editor,
            // While a script reads its next input, SIGINT ends the client.
            Mode::Script { .. } => UiState::Idle,
        }
    }

    /// Reports the outcome, then hangs up: the session may end as soon as the daemon sees the
    /// hangup.
    pub(crate) fn finish_session(mut self, outcome_tx: tokio::sync::oneshot::Sender<InputOutcome>) {
        let _ignored = outcome_tx.send(std::mem::take(&mut self.outcome));
        let _ignored = self.req_tx.send(ReplRequest {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            request: Some(repl_request::Request::Hangup(ReplHangup {})),
        });
    }

    /// Waits for the answer to `Open`, with the notices sent meanwhile (e.g. that the prelude
    /// could not be loaded). `None` if the session could not start (reported).
    pub(crate) fn wait_for_ready(&mut self) -> Option<(ReplReady, Vec<ReplNotice>)> {
        let mut notices = Vec::new();
        loop {
            match self.ui_rx.recv() {
                Ok(UiEvent::Ready(ready)) => return Some((ready, notices)),
                Ok(UiEvent::Notice(notice)) => notices.push(notice),
                Ok(UiEvent::Done(id, done)) if id == OPEN_ID => {
                    // The daemon could not start the session.
                    self.print_notices(&notices);
                    let rendered = render::render_done(&done, self.style()).map(|_| ());
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
        }
    }

    pub(crate) fn print_notices(&mut self, notices: &[ReplNotice]) {
        for notice in notices {
            let printed = render::print_notice(self.style(), notice);
            self.output(printed);
        }
    }

    /// Records the first output error. Returns whether the output was written.
    pub(crate) fn output(&mut self, result: buck2_error::Result<()>) -> bool {
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

    /// Prints `text` on stdout.
    fn print(&mut self, text: &str) -> Next {
        self.continue_if(render::print_text(text).and_then(|()| render::flush()))
    }

    /// Prints a note on stderr.
    fn note(&mut self, text: &str) -> Next {
        let printed = render::print_note(self.style(), text);
        self.continue_if(printed)
    }

    /// An input failed: interactively, the session goes on; a script stops, unless it goes on
    /// after errors.
    fn failed(&mut self) -> Next {
        match self.mode {
            Mode::Interactive => Next::Continue,
            Mode::Script { continue_on_error } => {
                self.outcome.failed = true;
                if continue_on_error {
                    Next::Continue
                } else {
                    Next::Stop
                }
            }
        }
    }

    /// An input failed with `message` (which starts with `error: `).
    fn error(&mut self, message: &str) -> Next {
        let printed = render::print_error(self.style(), message);
        if !self.output(printed) {
            return Next::Stop;
        }
        self.failed()
    }

    /// The files and the `-e` inputs of the command line, before any other input.
    pub(crate) fn run_first(&mut self, first: FirstInputs) -> Next {
        for file in &first.files {
            if let Next::Stop = self.preload(file) {
                return Next::Stop;
            }
        }
        for input in first.evals {
            if let Next::Stop = self.eval(input) {
                return Next::Stop;
            }
        }
        Next::Continue
    }

    /// A file of the command line: a `.bzl` or `.bxl` file is loaded as `:load` loads it (every
    /// public symbol becomes a binding); anything else is evaluated as one input.
    fn preload(&mut self, file: &str) -> Next {
        if file.ends_with(".bzl") || file.ends_with(".bxl") {
            let quoted = shlex::try_quote(file).map_or_else(|_| file.into(), |q| q);
            return self.eval(format!(":load {quoted}"));
        }
        let path = self.run_env.cwd.join(file);
        match std::fs::read_to_string(&path) {
            Ok(code) => match starts_with_command(&code) {
                Some(command) => self.error(&format!(
                    "error: `{file}` starts with the command `:{command}`, but a file is \
                     evaluated as one Starlark input, which cannot hold commands; feed it on \
                     stdin instead (`buck2 repl < {file}`), or give commands with -e"
                )),
                None => self.send(code.clone(), &code),
            },
            Err(e) => self.error(&format!("error: cannot read `{file}`: {e}")),
        }
    }

    /// Evaluates one input and renders its result.
    pub(crate) fn eval(&mut self, input: String) -> Next {
        match parse_command(&input) {
            Ok(Some(command)) => match command.spec.id {
                CommandId::Quit => return Next::Stop,
                CommandId::Help => {
                    return match help::help(&command.arg) {
                        Ok(text) => self.print(&text),
                        Err(message) => self.error(&format!("error: {message}")),
                    };
                }
                CommandId::Time => {
                    let timed = command.arg.into_owned();
                    return self.eval_timed(timed, &input);
                }
                CommandId::Complete => {
                    return match complete_command(&self.completer, &command.arg) {
                        Ok(json) => self.print(&json),
                        Err(message) => self.error(&format!("error: {message}")),
                    };
                }
                CommandId::Hist => {
                    return match parse_count(&command.arg) {
                        Ok(Some(0)) => Next::Continue,
                        Ok(count) => {
                            let text = self.history.render(count);
                            self.print(&text)
                        }
                        Err(e) => self.error(&format!("error: {e}; usage: {}", command.spec.usage)),
                    };
                }
                CommandId::Shell => {
                    let rendered = run::run_shell(
                        &command.arg,
                        &self.run_env,
                        &self.ui,
                        self.idle_state(),
                        self.style(),
                    );
                    return self.after_rendered(rendered);
                }
                CommandId::Edit => {
                    let arg = command.arg.into_owned();
                    return self.edit(&arg, &input);
                }
                CommandId::Set => {
                    let arg = command.arg.into_owned();
                    return self.set(&arg, &input);
                }
                _ => {}
            },
            Ok(None) => {}
            Err(e) => return self.error(&format!("error: {e}")),
        }
        let typed = input.clone();
        self.send(input, &typed)
    }

    /// `:time <input>`: evaluates the input, then prints how long it took.
    fn eval_timed(&mut self, mut input: String, typed: &str) -> Next {
        loop {
            match parse_command(&input) {
                // `:time :time x` times `x`.
                Ok(Some(command)) if command.spec.id == CommandId::Time => {
                    let timed = command.arg.into_owned();
                    input = timed;
                }
                // Nothing to time: handled here (`:set` of the client's settings too).
                Ok(Some(command))
                    if command.spec.handler == Handler::Client
                        || command.spec.id == CommandId::Set =>
                {
                    return self.eval(input);
                }
                Err(_) => return self.eval(input),
                Ok(_) => break,
            }
        }
        match self.request(input, Some(typed)) {
            Some((done, total)) => self.finish(&done, total, true),
            None => Next::Stop,
        }
    }

    /// Sends an input to the daemon (recorded for `:hist` as `typed`), and renders its result.
    fn send(&mut self, input: String, typed: &str) -> Next {
        match self.request(input, Some(typed)) {
            Some((done, total)) => self.finish(&done, total, false),
            None => Next::Stop,
        }
    }

    /// Sends an input to the daemon and waits for its result, with how long that took. An input
    /// `typed` by the user is numbered (the daemon names it `<repl:N>`) and recorded for `:hist`;
    /// a request made by the client itself (`None`) is not. `None` if the session ended.
    fn request(&mut self, input: String, typed: Option<&str>) -> Option<(ReplDone, Duration)> {
        let number = match typed {
            Some(typed) => {
                // The input may change what the daemon listed for completion.
                self.completer.clear_listings();
                self.number = self.number.saturating_add(1);
                self.history.push(self.number, typed);
                self.number
            }
            None => 0,
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = ReplRequest {
            id,
            request: Some(repl_request::Request::Eval(ReplEval { input, number })),
        };
        let start = Instant::now();
        self.ui.set(UiState::Busy { id, presses: 0 });
        if self.req_tx.send(request).is_err() {
            self.outcome.lost = true;
            return None;
        }
        let done = self.wait_for_done(id);
        self.ui.set(self.idle_state());
        let total = Instant::now() - start;
        match done {
            Some(done) => Some((done, total)),
            None => {
                self.outcome.lost = true;
                None
            }
        }
    }

    /// Waits for the result of request `id`, printing notices meanwhile (an output error is
    /// recorded). `None` if the session ended first.
    fn wait_for_done(&mut self, id: u64) -> Option<ReplDone> {
        loop {
            match self.ui_rx.recv() {
                Ok(UiEvent::Done(done_id, done)) if done_id == id => return Some(done),
                Ok(UiEvent::Notice(notice)) => {
                    let printed = render::print_notice(self.style(), &notice);
                    self.output(printed);
                }
                Ok(UiEvent::Done(..) | UiEvent::Ready(_)) => {}
                Ok(UiEvent::SessionEnded) | Err(_) => return None,
            }
        }
    }

    /// Renders the result of an input: runs the program of `:run`, then prints how long it
    /// took for `:time` (`timed`) and with `:set timing on`.
    fn finish(&mut self, done: &ReplDone, total: Duration, timed: bool) -> Next {
        if self.outcome.output_error.is_some() {
            // A notice could not be printed.
            return Next::Stop;
        }
        let show_timing = timed || self.settings.timing == Switch::On;
        let style = if show_timing {
            self.style().without_durations()
        } else {
            self.style()
        };
        let rendered = render::render_done(done, style).and_then(|r| {
            let r = match r {
                Rendered::Run(run) => {
                    run::run_program(&run, &self.run_env, &self.ui, self.idle_state(), style)?
                }
                r => r,
            };
            if show_timing {
                render::print_timing(style, total, done)?;
            }
            render::flush().map(|()| r)
        });
        self.after_rendered(rendered)
    }

    /// Whether the session goes on after an input that was rendered.
    fn after_rendered(&mut self, rendered: buck2_error::Result<Rendered>) -> Next {
        let rendered = match rendered {
            Ok(rendered) => rendered,
            Err(e) => {
                self.output(Err(e));
                return Next::Stop;
            }
        };
        match rendered {
            Rendered::Ok | Rendered::Run(_) => Next::Continue,
            Rendered::Failed | Rendered::Interrupted if self.shutdown.is_shutting_down() => {
                // The daemon cancels the input in flight when it shuts down.
                self.outcome.after_shutdown = true;
                Next::Stop
            }
            Rendered::Failed => self.failed(),
            Rendered::Interrupted => match self.mode {
                Mode::Interactive => Next::Continue,
                Mode::Script { .. } => {
                    self.outcome.interrupted = true;
                    Next::Stop
                }
            },
        }
    }

    /// `:set [key [value...]]`: the client's settings are set here, the daemon's by the daemon
    /// (`input` is sent as it is).
    fn set(&mut self, arg: &str, input: &str) -> Next {
        let usage = command(CommandId::Set).map_or("", |c| c.usage);
        let args = match parse_set_args(arg) {
            Ok(args) => args,
            Err(e) => return self.error(&format!("error: {e}; usage: {usage}")),
        };
        match args.key {
            None => {
                // The daemon's settings, then the client's.
                if let Next::Stop = self.send(input.to_owned(), input) {
                    return Next::Stop;
                }
                let text = self.settings.show(None, &self.completer);
                self.print(&text)
            }
            Some(spec) if spec.side == SettingSide::Server => self.send(input.to_owned(), input),
            Some(spec) => match args.value {
                None => {
                    let text = self.settings.show(Some(spec), &self.completer);
                    self.print(&text)
                }
                Some(value) => match self.settings.set(spec, &value, &self.completer) {
                    Ok(()) => {
                        let text = format!(
                            "note: {} is now {}",
                            spec.name,
                            self.settings.value(spec.name, &self.completer)
                        );
                        self.note(&text)
                    }
                    Err(message) => self.error(&format!("error: {message}")),
                },
            },
        }
    }

    /// `:edit [path|target]`.
    fn edit(&mut self, arg: &str, typed: &str) -> Next {
        let words = match split_args(arg) {
            Ok(words) => words,
            Err(e) => return self.error(&format!("error: {e}")),
        };
        let mut words = words.into_iter();
        match (words.next(), words.next()) {
            (None, _) => self.edit_scratch(),
            (Some(what), None) => self.edit_file(&what, typed),
            (Some(_), Some(_)) => {
                self.error("error: `:edit` takes one path or target; usage: :edit [path|target]")
            }
        }
    }

    /// `:edit <path|target>`: a file (relative to the working directory), the build file of a
    /// target, or a module (`//pkg:x.bzl`). A module is loaded again afterwards if the session
    /// loads it.
    fn edit_file(&mut self, what: &str, typed: &str) -> Next {
        if what.is_empty() {
            return self.error(
                "error: `:edit` takes a path or a target, not an empty word; usage: :edit \
                 [path|target]",
            );
        }
        let path = self.run_env.cwd.join(what);
        let is_label = what.contains(':') || what.contains("//") || what.starts_with('@');
        // `//pkg:x` and `//pkg` are labels, never the paths `/pkg:x` and `/pkg`.
        let is_path = !what.starts_with("//") && (path.exists() || !is_label);
        if is_path && path.is_dir() {
            return self.error(&format!(
                "error: `{what}` is a directory; `:edit` edits a file"
            ));
        }
        let target = if is_path {
            EditTarget {
                module: is_module(&path),
                path,
                line: None,
            }
        } else {
            match self.locate(what) {
                Ok(target) => target,
                Err(next) => return next,
            }
        };
        if let Err(message) = run::run_editor(
            &target.path,
            target.line,
            &self.run_env,
            &self.ui,
            self.idle_state(),
        ) {
            return self.error(&format!("error: {message}"));
        }
        if !target.module {
            return Next::Continue;
        }
        // Loaded again if the session loads it (directly or through another module).
        let path = target.path.to_string_lossy().into_owned();
        let quoted = shlex::try_quote(&path).map_or_else(|_| path.clone().into(), |q| q);
        self.send(format!(":__edited {quoted}"), typed)
    }

    /// Where a target is defined, or where a module is, as the daemon says.
    fn locate(&mut self, what: &str) -> Result<EditTarget, Next> {
        let quoted = shlex::try_quote(what).map_or_else(|_| what.into(), |q| q);
        let Some((done, _)) = self.request(format!(":__locate {quoted}"), None) else {
            return Err(Next::Stop);
        };
        let location = match &done.outcome {
            Some(repl_done::Outcome::Value(value)) => {
                // Not shown by the next input: the daemon compares with this request.
                let printed = render::print_sources_changed(&done, self.style());
                if !self.output(printed) {
                    return Err(Next::Stop);
                }
                serde_json::from_str::<serde_json::Value>(&value.text).ok()
            }
            Some(repl_done::Outcome::Error(_)) => {
                let rendered = render::render_done(&done, self.style());
                return Err(self.after_rendered(rendered));
            }
            _ => None,
        };
        // Why the location is not the one asked for (the package does not load).
        if let Some(warning) = location
            .as_ref()
            .and_then(|l| l.get("warning"))
            .and_then(|w| w.as_str())
        {
            let printed = render::print_note(self.style(), &format!("warning: {warning}"));
            if !self.output(printed) {
                return Err(Next::Stop);
            }
        }
        let path = location
            .as_ref()
            .and_then(|l| l.get("path"))
            .and_then(|p| p.as_str());
        match (path, &location) {
            (Some(path), Some(location)) => Ok(EditTarget {
                path: PathBuf::from(path),
                line: location
                    .get("line")
                    .and_then(|l| l.as_u64())
                    .and_then(|l| usize::try_from(l).ok()),
                module: location
                    .get("module")
                    .and_then(|m| m.as_bool())
                    .unwrap_or(false),
            }),
            _ => Err(self.error(&format!("error: the daemon did not say where `{what}` is"))),
        }
    }

    /// `:edit`: edits the scratch buffer (as it was last left) in a temporary file, then
    /// evaluates it as one input.
    fn edit_scratch(&mut self) -> Next {
        let (path, file) = match scratch_file() {
            Ok(created) => created,
            Err(e) => {
                return self.error(&format!("error: cannot create the scratch buffer: {e}"));
            }
        };
        let written = {
            let mut file = file;
            file.write_all(self.scratch.as_bytes())
        };
        let edited = match written {
            Err(e) => Err(format!(
                "cannot write the scratch buffer `{}`: {e}",
                path.display()
            )),
            Ok(()) => run::run_editor(&path, None, &self.run_env, &self.ui, self.idle_state())
                .and_then(|()| {
                    std::fs::read_to_string(&path).map_err(|e| {
                        format!("cannot read the scratch buffer `{}`: {e}", path.display())
                    })
                }),
        };
        let _ignored = std::fs::remove_file(&path);
        let code = match edited {
            Ok(code) => code,
            Err(message) => return self.error(&format!("error: {message}")),
        };
        self.scratch = code.clone();
        if code.trim().is_empty() {
            return self.note("note: the scratch buffer is empty: nothing to run");
        }
        if let Some(command) = starts_with_command(&code) {
            return self.error(&format!(
                "error: the scratch buffer starts with the command `:{command}`, but it is \
                 evaluated as one Starlark input, which cannot hold commands (it is kept: \
                 `:edit` opens it again)"
            ));
        }
        // Recorded as the code (for `:hist`), which is what `<repl:N>` names.
        self.send(code.clone(), &code)
    }
}

/// The command a file or the scratch buffer starts with, if it does: evaluated as one input, it
/// would be taken as that command, with the rest of the text as its argument.
fn starts_with_command(code: &str) -> Option<&str> {
    split_command_token(code).map(|token| token.token)
}

/// Whether the file is a module that `load` loads.
fn is_module(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "bzl" || e == "bxl")
}

/// A new file for the scratch buffer of `:edit`, in the temporary directory (never an existing
/// one, so it is not a link planted there).
fn scratch_file() -> std::io::Result<(PathBuf, std::fs::File)> {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let mut last_error = None;
    for attempt in 0..16u32 {
        let path = std::env::temp_dir().join(format!(
            "buck2-repl-{}-{nanos}-{attempt}.bxl",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(e) => last_error = Some(e),
        }
    }
    Err(last_error.unwrap_or_else(|| std::io::Error::other("no name is free")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_history() {
        let mut history = InputHistory::default();
        assert_eq!(history.render(None), "(no inputs yet)");
        history.push(1, "x = 1\n");
        history.push(2, "def f():\n    return 2");
        history.push(3, "f()");
        assert_eq!(
            history.render(None),
            "   1  x = 1\n   2  def f():\n          return 2\n   3  f()"
        );
        assert_eq!(history.render(Some(1)), "   3  f()");
        assert_eq!(history.render(Some(0)), "");
        for i in 0..2000 {
            history.push(i, "x");
        }
        assert_eq!(history.inputs.len(), MAX_HISTORY);
        let mut history = InputHistory::default();
        history.push(1, &"x".repeat(MAX_HISTORY_INPUT + 10));
        assert!(history.render(None).ends_with('…'));
        assert!(is_module(Path::new("a/b.bzl")));
        assert!(!is_module(Path::new("a/TARGETS")));
        assert_eq!(starts_with_command("\n  :set x\ny = 1"), Some("set"));
        assert_eq!(starts_with_command(":b //:x\nprint(1)"), Some("b"));
        assert_eq!(starts_with_command("x = 1\n:b //:x"), None);
        assert_eq!(starts_with_command("# :b\nx = 1"), None);
    }
}
