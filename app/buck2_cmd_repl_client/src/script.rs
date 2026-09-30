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

use buck2_cli_proto::ReplDone;
use buck2_cli_proto::ReplEval;
use buck2_cli_proto::ReplHangup;
use buck2_cli_proto::ReplRequest;
use buck2_cli_proto::repl_request;
use buck2_client_ctx::exit_result::ExitResult;
use buck2_error::ExitCode;
use buck2_repl_syntax::chunker::Chunker;
use buck2_repl_syntax::commands::CommandId;
use buck2_repl_syntax::commands::parse_command;

use crate::render;
use crate::render::Rendered;
use crate::session::OPEN_ID;
use crate::session::ShutdownHangup;
use crate::session::UiEvent;

pub(crate) enum ScriptInputs {
    /// The `-e` values, one input each.
    Args(Vec<String>),
    /// Lines from stdin, split into inputs by the [`Chunker`].
    Stdin,
}

/// How a script went.
#[derive(Debug, Default)]
pub(crate) struct ScriptOutcome {
    /// An input failed.
    failed: bool,
    /// An input was interrupted.
    interrupted: bool,
    /// The session ended before the inputs did.
    lost: bool,
    /// Output could not be written (e.g. stdout is a closed pipe), which stopped the script.
    output_error: Option<buck2_error::Error>,
    /// An input failed or was interrupted after the daemon said that it was shutting down
    /// (which cancels the input in flight).
    after_shutdown: bool,
}

impl ScriptOutcome {
    /// The script stopped because the daemon shut down, which explains the outcome better
    /// than its own exit code.
    pub(crate) fn ended_by_shutdown(&self) -> bool {
        self.output_error.is_none() && (self.after_shutdown || self.lost)
    }

    pub(crate) fn exit_result(self) -> ExitResult {
        if let Some(e) = self.output_error {
            // A closed pipe exits like other commands do (quietly, with its own exit code).
            ExitResult::err(e)
        } else if self.lost {
            ExitResult::err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "the repl session ended before all inputs were evaluated"
            ))
        } else if self.interrupted {
            ExitResult::signal_interrupt()
        } else if self.failed {
            ExitResult::status_with_emitted_errors(ExitCode::UserError, Vec::new())
        } else {
            ExitResult::success()
        }
    }
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
    pub(crate) outcome_tx: tokio::sync::oneshot::Sender<ScriptOutcome>,
    pub(crate) shutdown: ShutdownHangup,
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
            outcome_tx,
            shutdown,
        } = self;
        let mut session = Session {
            continue_on_error,
            req_tx,
            ui_rx,
            next_id,
            shutdown,
            number: 0,
            outcome: ScriptOutcome::default(),
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
    shutdown: ShutdownHangup,
    /// Number of inputs sent so far; the daemon names input N `<repl:N>`.
    number: u32,
    outcome: ScriptOutcome,
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
                            let printed =
                                render::print_error(&format!("error: cannot read stdin: {e}"));
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
                Ok(UiEvent::Ready) => return true,
                Ok(UiEvent::Notice(notice)) => {
                    if !self.output(render::print_notice(&notice)) {
                        return false;
                    }
                }
                Ok(UiEvent::Done(id, done)) if id == OPEN_ID => {
                    // The daemon could not start the session.
                    let rendered = render::render_done(&done).map(|_| ());
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
            Ok(Some(command)) if command.spec.id == CommandId::Quit => return Next::Stop,
            Ok(_) => {}
            Err(e) => {
                if !self.output(render::print_error(&format!("error: {e}"))) {
                    return Next::Stop;
                }
                return self.failed();
            }
        }

        self.number = self.number.saturating_add(1);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = ReplRequest {
            id,
            request: Some(repl_request::Request::Eval(ReplEval {
                input,
                number: self.number,
            })),
        };
        if self.req_tx.send(request).is_err() {
            self.outcome.lost = true;
            return Next::Stop;
        }
        let Some(done) = self.wait_for_done(id) else {
            self.outcome.lost = true;
            return Next::Stop;
        };
        if self.outcome.output_error.is_some() {
            // A notice could not be printed.
            return Next::Stop;
        }
        let rendered = render::render_done(&done);
        let rendered = match rendered.and_then(|r| render::flush().map(|()| r)) {
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
            Rendered::Ok => Next::Continue,
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
                    let printed = render::print_notice(&notice);
                    self.output(printed);
                }
                Ok(UiEvent::Done(..) | UiEvent::Ready) => {}
                Ok(UiEvent::SessionEnded) | Err(_) => return None,
            }
        }
    }
}
