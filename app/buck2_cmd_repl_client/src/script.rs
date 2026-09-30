/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Non-interactive mode: evaluates the `-e` inputs, or the inputs piped on stdin (split with
//! the [`Chunker`] as lines arrive, so that a program can drive the session), one at a time.

use std::io::BufRead;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use buck2_cli_proto::ReplDone;
use buck2_cli_proto::ReplEval;
use buck2_cli_proto::ReplHangup;
use buck2_cli_proto::ReplRequest;
use buck2_cli_proto::repl_request;
use buck2_repl_syntax::chunker::Chunker;
use buck2_repl_syntax::commands::CommandId;
use buck2_repl_syntax::commands::Handler;
use buck2_repl_syntax::commands::parse_command;

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

pub(crate) enum ScriptInputs {
    /// The `-e` values, one input each.
    Args(Vec<String>),
    /// Lines from stdin, split into inputs by the [`Chunker`].
    Stdin,
}

/// Stop or go on after an input.
enum Next {
    Continue,
    Stop,
}

pub(crate) struct ScriptMode {
    pub(crate) inputs: ScriptInputs,
    pub(crate) continue_on_error: bool,
    pub(crate) req_tx: tokio::sync::mpsc::UnboundedSender<ReplRequest>,
    pub(crate) ui_rx: std::sync::mpsc::Receiver<UiEvent>,
    pub(crate) next_id: Arc<AtomicU64>,
    pub(crate) completer: Arc<Completer>,
    pub(crate) ui: SharedUi,
    pub(crate) outcome_tx: tokio::sync::oneshot::Sender<InputOutcome>,
    pub(crate) shutdown: ShutdownHangup,
    pub(crate) run_env: RunEnv,
}

impl ScriptMode {
    /// Runs on the input thread. Reports the outcome, then hangs up.
    pub(crate) fn run(self) {
        let ScriptMode {
            inputs,
            continue_on_error,
            req_tx,
            ui_rx,
            next_id,
            completer,
            ui,
            outcome_tx,
            shutdown,
            run_env,
        } = self;
        let mut session = Session {
            continue_on_error,
            req_tx,
            ui_rx,
            next_id,
            completer,
            ui,
            shutdown,
            run_env,
            number: 0,
            outcome: InputOutcome::default(),
        };
        session.run(inputs);
        // Report the outcome before hanging up: the session may end as soon as the daemon
        // sees the hangup.
        let _ignored = outcome_tx.send(std::mem::take(&mut session.outcome));
        let _ignored = session.req_tx.send(ReplRequest {
            id: session.next_id.fetch_add(1, Ordering::Relaxed),
            request: Some(repl_request::Request::Hangup(ReplHangup {})),
        });
    }
}

struct Session {
    continue_on_error: bool,
    req_tx: tokio::sync::mpsc::UnboundedSender<ReplRequest>,
    ui_rx: std::sync::mpsc::Receiver<UiEvent>,
    next_id: Arc<AtomicU64>,
    completer: Arc<Completer>,
    ui: SharedUi,
    shutdown: ShutdownHangup,
    run_env: RunEnv,
    /// Number of inputs sent so far; the daemon names input N `<repl:N>`.
    number: u32,
    outcome: InputOutcome,
}

impl Session {
    fn run(&mut self, inputs: ScriptInputs) {
        if !self.wait_for_ready() {
            return;
        }
        match inputs {
            ScriptInputs::Args(inputs) => {
                for input in inputs {
                    if let Next::Stop = self.eval(input) {
                        return;
                    }
                }
            }
            ScriptInputs::Stdin => {
                let mut chunker = Chunker::new();
                for line in std::io::stdin().lock().lines() {
                    let line = match line {
                        Ok(line) => line,
                        Err(e) => {
                            let printed = render::print_error(
                                Style::script(),
                                &format!("error: cannot read stdin: {e}"),
                            );
                            self.output(printed);
                            self.outcome.failed = true;
                            return;
                        }
                    };
                    for chunk in chunker.push_line(&line) {
                        if let Next::Stop = self.eval(chunk) {
                            return;
                        }
                    }
                }
                if let Some(chunk) = chunker.finish() {
                    let _ignored = self.eval(chunk);
                }
            }
        }
    }

    /// Waits for the answer to `Open`. Returns whether the session is ready.
    fn wait_for_ready(&mut self) -> bool {
        loop {
            match self.ui_rx.recv() {
                Ok(UiEvent::Ready(_)) => return true,
                Ok(UiEvent::Notice(notice)) => {
                    if !self.output(render::print_notice(Style::script(), &notice)) {
                        return false;
                    }
                }
                Ok(UiEvent::Done(id, done)) if id == OPEN_ID => {
                    // The daemon could not start the session.
                    let rendered = render::render_done(&done, Style::script()).map(|_| ());
                    self.output(rendered);
                    self.outcome.failed = true;
                    return false;
                }
                Ok(UiEvent::Done(..)) => {}
                Ok(UiEvent::SessionEnded) | Err(_) => {
                    self.outcome.lost = true;
                    return false;
                }
            }
        }
    }

    /// Evaluates one input and renders its result.
    fn eval(&mut self, input: String) -> Next {
        match parse_command(&input) {
            Ok(Some(command)) => match command.spec.id {
                CommandId::Quit => return Next::Stop,
                CommandId::Help => {
                    return match help::help(&command.arg) {
                        Ok(text) => {
                            if self.output(render::print_text(&text).and_then(|()| render::flush()))
                            {
                                Next::Continue
                            } else {
                                Next::Stop
                            }
                        }
                        Err(message) => {
                            let printed =
                                render::print_error(Style::script(), &format!("error: {message}"));
                            if !self.output(printed) {
                                return Next::Stop;
                            }
                            self.failed()
                        }
                    };
                }
                CommandId::Time => {
                    let timed = command.arg.into_owned();
                    return self.eval_timed(timed);
                }
                CommandId::Complete => {
                    return match complete_command(&self.completer, &command.arg) {
                        Ok(json) => {
                            if self.output(render::print_text(&json).and_then(|()| render::flush()))
                            {
                                Next::Continue
                            } else {
                                Next::Stop
                            }
                        }
                        Err(message) => {
                            let printed =
                                render::print_error(Style::script(), &format!("error: {message}"));
                            if !self.output(printed) {
                                return Next::Stop;
                            }
                            self.failed()
                        }
                    };
                }
                _ => {}
            },
            Ok(None) => {}
            Err(e) => {
                if !self.output(render::print_error(Style::script(), &format!("error: {e}"))) {
                    return Next::Stop;
                }
                return self.failed();
            }
        }
        match self.request(input) {
            Some(done) => self.finish(&done, None),
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
                self.finish(&done, Some(total))
            }
            None => Next::Stop,
        }
    }

    /// Sends an input to the daemon and waits for its result. `None` if the session ended.
    fn request(&mut self, input: String) -> Option<ReplDone> {
        // The input may change what the daemon listed for completion.
        self.completer.clear_listings();
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
        // While the script reads its next input, SIGINT ends the client.
        self.ui.set(UiState::Idle);
        if done.is_none() {
            self.outcome.lost = true;
        }
        done
    }

    /// Renders the result of an input (and how long it took, for `:time`).
    fn finish(&mut self, done: &ReplDone, timing: Option<Duration>) -> Next {
        if self.outcome.output_error.is_some() {
            // A notice could not be printed.
            return Next::Stop;
        }
        let rendered = render::render_done(done, Style::script()).and_then(|r| {
            let r = match r {
                Rendered::Run(run) => run::run_program(
                    &run,
                    &self.run_env,
                    &self.ui,
                    UiState::Idle,
                    Style::script(),
                )?,
                r => r,
            };
            if let Some(total) = timing {
                render::print_timing(Style::script(), total, done)?;
            }
            render::flush().map(|()| r)
        });
        let rendered = match rendered {
            Ok(rendered) => rendered,
            Err(e) => {
                self.output(Err(e));
                return Next::Stop;
            }
        };
        if !matches!(rendered, Rendered::Ok) && self.shutdown.is_shutting_down() {
            // The daemon cancels the input in flight when it shuts down.
            self.outcome.after_shutdown = true;
            return Next::Stop;
        }
        match rendered {
            Rendered::Ok | Rendered::Run(_) => Next::Continue,
            Rendered::Failed => self.failed(),
            Rendered::Interrupted => {
                self.outcome.interrupted = true;
                Next::Stop
            }
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

    fn failed(&mut self) -> Next {
        self.outcome.failed = true;
        if self.continue_on_error {
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
                    let printed = render::print_notice(Style::script(), &notice);
                    self.output(printed);
                }
                Ok(UiEvent::Done(..) | UiEvent::Ready(_)) => {}
                Ok(UiEvent::SessionEnded) | Err(_) => return None,
            }
        }
    }
}
