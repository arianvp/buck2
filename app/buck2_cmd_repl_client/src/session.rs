/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! The session: the daemon call on the client's runtime, the thread that reads inputs (the
//! interactive [`editor`](crate::editor) or the [`script`](crate::script) reader), and the
//! SIGINT handler.
//!
//! The input thread sends requests through `req_tx` (the request stream of the call) and
//! waits for their results on `ui_rx`, which [`ReplHandler`] feeds from the call's partial
//! results. Output (`ReplOutput`) is written by the handler as it arrives, so it is always
//! printed before the `ReplDone` of its request is rendered.

use std::io::IsTerminal;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use buck2_cli_proto::ClientContext;
use buck2_cli_proto::ReplCompletions;
use buck2_cli_proto::ReplDone;
use buck2_cli_proto::ReplHangup;
use buck2_cli_proto::ReplInterrupt;
use buck2_cli_proto::ReplMessage;
use buck2_cli_proto::ReplNotice;
use buck2_cli_proto::ReplOpen;
use buck2_cli_proto::ReplOutput;
use buck2_cli_proto::ReplReady;
use buck2_cli_proto::ReplRequest;
use buck2_cli_proto::repl_message;
use buck2_cli_proto::repl_output;
use buck2_cli_proto::repl_request;
use buck2_client_ctx::command_outcome::CommandOutcome;
use buck2_client_ctx::daemon::client::BuckdClientConnector;
use buck2_client_ctx::events_ctx::EventsCtx;
use buck2_client_ctx::events_ctx::PartialResultCtx;
use buck2_client_ctx::events_ctx::PartialResultHandler;
use buck2_client_ctx::exit_result::ClientIoError;
use buck2_client_ctx::exit_result::ExitResult;
use buck2_client_ctx::subscribers::subscriber::EventSubscriber;
use buck2_error::ExitCode;
use buck2_events::BuckEvent;
use buck2_util::threads::thread_spawn;
use dupe::Dupe;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::ReplCommand;
use crate::complete::Completer;
use crate::editor::EditorMode;
use crate::editor::history_path;
use crate::run::RunEnv;
use crate::script::ScriptInputs;
use crate::script::ScriptMode;

/// The id of the `Open` request. Later requests count up from here.
pub(crate) const OPEN_ID: u64 = 1;

/// Hangs the session up when the daemon announces that it is shutting down (e.g. `buck2 kill`).
///
/// The daemon waits for its open streams before it exits, and an idle session would otherwise
/// keep its stream open until the daemon gives up waiting and is killed.
#[derive(Clone, Debug, Default, Dupe)]
pub(crate) struct ShutdownHangup(Arc<Mutex<ShutdownState>>);

#[derive(Debug, Default)]
struct ShutdownState {
    /// Where to send the hangup, once the session has started. Weak, so that it does not keep
    /// the request stream open.
    requests: Option<(
        tokio::sync::mpsc::WeakUnboundedSender<ReplRequest>,
        Arc<AtomicU64>,
    )>,
    /// Why the daemon is shutting down, once it has said so.
    reason: Option<String>,
}

impl ShutdownHangup {
    fn state(&self) -> std::sync::MutexGuard<'_, ShutdownState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn arm(
        &self,
        req_tx: &tokio::sync::mpsc::UnboundedSender<ReplRequest>,
        next_id: Arc<AtomicU64>,
    ) {
        self.state().requests = Some((req_tx.downgrade(), next_id));
    }

    fn hang_up(&self, reason: &str) {
        let mut state = self.state();
        if state.reason.is_none() {
            state.reason = Some(reason.to_owned());
        }
        if let Some((req_tx, next_id)) = state.requests.take()
            && let Some(req_tx) = req_tx.upgrade()
        {
            // Fails only if the call is already over.
            let _ignored = req_tx.send(ReplRequest {
                id: next_id.fetch_add(1, Ordering::Relaxed),
                request: Some(repl_request::Request::Hangup(ReplHangup {})),
            });
        }
    }

    fn reason(&self) -> Option<String> {
        self.state().reason.clone()
    }

    /// The daemon has said that it is shutting down.
    pub(crate) fn is_shutting_down(&self) -> bool {
        self.state().reason.is_some()
    }

    pub(crate) fn subscriber(&self) -> Box<dyn EventSubscriber> {
        Box::new(ShutdownSubscriber(self.dupe()))
    }
}

struct ShutdownSubscriber(ShutdownHangup);

#[async_trait]
impl EventSubscriber for ShutdownSubscriber {
    fn name(&self) -> &'static str {
        "repl-shutdown"
    }

    async fn handle_events(&mut self, events: &[Arc<BuckEvent>]) -> buck2_error::Result<()> {
        for event in events {
            if let buck2_data::buck_event::Data::Instant(instant) = event.data() {
                if let Some(buck2_data::instant_event::Data::DaemonShutdown(shutdown)) =
                    &instant.data
                {
                    self.0.hang_up(&shutdown.reason);
                }
            }
        }
        Ok(())
    }
}

/// What the input thread learns from the daemon.
pub(crate) enum UiEvent {
    /// The answer to `Open`.
    Ready(ReplReady),
    Done(u64, ReplDone),
    Notice(ReplNotice),
    /// The daemon call is over (or abandoned): nothing else will arrive.
    SessionEnded,
}

/// What the terminal is doing, which decides what SIGINT (Ctrl-C) does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UiState {
    /// No input is running and the line editor does not own the terminal: the session is
    /// starting, or a script is waiting for its next input. SIGINT ends the client.
    Idle,
    /// The line editor owns the terminal and no input is running: it is reading (its raw mode
    /// turns Ctrl-C into a key, so SIGINT only comes from elsewhere, e.g. `kill -INT`) or
    /// showing a result. SIGINT is ignored.
    Editor,
    /// Request `id` is running. SIGINT interrupts it, warns, then ends the client.
    Busy { id: u64, presses: u32 },
    /// The program of `:run` is running in the foreground. SIGINT is for it (the terminal sends
    /// it to the whole process group): the session ignores it.
    Child,
}

/// The state of the terminal, shared by the input thread, the SIGINT handler and the session.
#[derive(Clone, Dupe)]
pub(crate) struct SharedUi(Arc<Mutex<UiInner>>);

struct UiInner {
    state: UiState,
    /// The line editor is reading a line, and only notices the end of the session after it.
    reading: bool,
    /// The daemon call is over: the input thread must not wait for input any more.
    session_ended: bool,
}

impl SharedUi {
    fn new() -> Self {
        SharedUi(Arc::new(Mutex::new(UiInner {
            state: UiState::Idle,
            reading: false,
            session_ended: false,
        })))
    }

    fn lock(&self) -> MutexGuard<'_, UiInner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn set(&self, state: UiState) {
        self.lock().state = state;
    }

    /// The line editor starts reading a line, unless the session is over. Returns whether it
    /// is not (so that the editor may read).
    pub(crate) fn start_reading(&self) -> bool {
        let mut inner = self.lock();
        if inner.session_ended {
            return false;
        }
        inner.state = UiState::Editor;
        inner.reading = true;
        true
    }

    /// The line editor has read a line.
    pub(crate) fn stop_reading(&self) {
        self.lock().reading = false;
    }

    /// The daemon call is over.
    pub(crate) fn session_ended(&self) -> bool {
        self.lock().session_ended
    }

    /// Marks the session as over. Returns whether the line editor is reading, in which case it
    /// only notices after its current line.
    fn end_session(&self) -> bool {
        let mut inner = self.lock();
        inner.session_ended = true;
        inner.reading
    }
}

/// How the inputs went, reported by the input thread.
#[derive(Debug, Default)]
pub(crate) struct InputOutcome {
    /// An input failed (or, interactively, the session could not start or input could not be
    /// read).
    pub(crate) failed: bool,
    /// An input was interrupted.
    pub(crate) interrupted: bool,
    /// The session ended before the inputs did.
    pub(crate) lost: bool,
    /// Output could not be written (e.g. stdout is a closed pipe), which stopped the inputs.
    pub(crate) output_error: Option<buck2_error::Error>,
    /// An input failed or was interrupted after the daemon said that it was shutting down
    /// (which cancels the input in flight).
    pub(crate) after_shutdown: bool,
}

impl InputOutcome {
    /// The inputs stopped because the daemon shut down, which explains the outcome better than
    /// its own exit code.
    fn ended_by_shutdown(&self) -> bool {
        self.output_error.is_none() && (self.after_shutdown || self.lost)
    }

    fn exit_result(self) -> ExitResult {
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

/// Routes the call's partial results.
struct ReplHandler {
    ui_tx: std::sync::mpsc::Sender<UiEvent>,
    compl_tx: std::sync::mpsc::Sender<(u64, ReplCompletions)>,
}

#[async_trait]
impl PartialResultHandler for ReplHandler {
    type PartialResult = ReplMessage;

    async fn handle_partial_result(
        &mut self,
        _ctx: PartialResultCtx<'_>,
        message: Self::PartialResult,
    ) -> buck2_error::Result<()> {
        let id = message.id;
        // Send errors mean the input thread is gone, which the call's result reports.
        match message.message {
            Some(repl_message::Message::Output(output)) => write_output(&output)?,
            Some(repl_message::Message::Ready(ready)) => {
                let _ignored = self.ui_tx.send(UiEvent::Ready(ready));
            }
            Some(repl_message::Message::Done(done)) => {
                let _ignored = self.ui_tx.send(UiEvent::Done(id, done));
            }
            Some(repl_message::Message::Notice(notice)) => {
                let _ignored = self.ui_tx.send(UiEvent::Notice(notice));
            }
            Some(repl_message::Message::Completions(completions)) => {
                let _ignored = self.compl_tx.send((id, completions));
            }
            None => {}
        }
        Ok(())
    }
}

fn write_output(output: &ReplOutput) -> buck2_error::Result<()> {
    match output.channel() {
        repl_output::Channel::Stdout => {
            buck2_client_ctx::stdio::print_bytes(&output.data)?;
            buck2_client_ctx::stdio::flush()
        }
        repl_output::Channel::Stderr => {
            let mut stderr = std::io::stderr().lock();
            stderr
                .write_all(&output.data)
                .and_then(|()| stderr.flush())
                .map_err(|e| ClientIoError::from(e).into())
        }
    }
}

/// Handles SIGINT (Ctrl-C) while the session runs, according to the [`UiState`]. Returns when
/// the client should give up on the session and exit.
async fn sigint_loop(
    ui: SharedUi,
    req_tx: tokio::sync::mpsc::WeakUnboundedSender<ReplRequest>,
    next_id: Arc<AtomicU64>,
) {
    // The terminal echoes `^C` itself: write over it.
    let prefix = if std::io::stderr().is_terminal() {
        "\r"
    } else {
        ""
    };
    loop {
        if tokio::signal::ctrl_c().await.is_err() {
            // No handler could be installed: SIGINT keeps its default action.
            return futures::future::pending().await;
        }
        let mut inner = ui.lock();
        let presses = match &mut inner.state {
            UiState::Idle => return,
            UiState::Editor | UiState::Child => continue,
            UiState::Busy { id, presses } => {
                *presses = presses.saturating_add(1);
                if *presses == 1
                    && let Some(req_tx) = req_tx.upgrade()
                {
                    // Fails only if the call is over, which ends this loop anyway.
                    let _ignored = req_tx.send(ReplRequest {
                        id: next_id.fetch_add(1, Ordering::Relaxed),
                        request: Some(repl_request::Request::Interrupt(ReplInterrupt {
                            target_id: *id,
                        })),
                    });
                }
                *presses
            }
        };
        drop(inner);
        // Output errors do not matter here: the input thread reports them.
        match presses {
            1 => {
                let _ignored = buck2_client_ctx::eprintln!("{}^C interrupting…", prefix);
            }
            2 => {
                let _ignored = buck2_client_ctx::eprintln!(
                    "{}^C still cancelling — the daemon may be waiting for another buck2 \
                     command; ^C again to quit",
                    prefix
                );
            }
            _ => return,
        }
    }
}

pub(crate) async fn run(
    cmd: ReplCommand,
    context: ClientContext,
    buckd: &mut BuckdClientConnector,
    events_ctx: &mut EventsCtx,
) -> ExitResult {
    let interactive = cmd.eval.is_empty() && std::io::stdin().is_terminal();
    let history = if interactive {
        match history_path(cmd.no_history) {
            Ok(history) => history,
            Err(e) => return ExitResult::err(e),
        }
    } else {
        None
    };

    let build_opts = cmd.build_opts.to_proto();
    let run_env = RunEnv {
        trace_id: context.trace_id.clone(),
        cwd: PathBuf::from(&context.working_dir),
        interactive,
    };
    let (req_tx, req_rx) = tokio::sync::mpsc::unbounded_channel::<ReplRequest>();
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();
    let (compl_tx, compl_rx) = std::sync::mpsc::channel::<(u64, ReplCompletions)>();
    let (outcome_tx, mut outcome_rx) = tokio::sync::oneshot::channel::<InputOutcome>();
    let next_id = Arc::new(AtomicU64::new(OPEN_ID + 1));
    let completer = match Completer::new(
        req_tx.clone(),
        compl_rx,
        next_id.dupe(),
        run_env.cwd.clone(),
    ) {
        Ok(completer) => Arc::new(completer),
        Err(e) => return ExitResult::err(e),
    };
    let ui = SharedUi::new();
    cmd.shutdown.arm(&req_tx, next_id.dupe());

    let open = ReplRequest {
        id: OPEN_ID,
        request: Some(repl_request::Request::Open(ReplOpen {
            target_cfg: Some(cmd.target_cfg.target_cfg()),
            max_heap_bytes: cmd.max_heap_mb.saturating_mul(1 << 20),
            json_values: cmd.json,
            preload: Vec::new(),
        })),
    };
    // Cannot fail: `req_rx` is alive.
    let _ignored = req_tx.send(open);

    let shutdown = cmd.shutdown.dupe();
    let sigint = sigint_loop(ui.dupe(), req_tx.downgrade(), next_id.dupe());
    let thread = if interactive {
        let editor = EditorMode {
            req_tx,
            ui_rx,
            next_id,
            completer,
            ui: ui.dupe(),
            history,
            outcome_tx,
            shutdown: shutdown.dupe(),
            run_env,
        };
        thread_spawn("repl-editor", move || editor.run())
    } else {
        let inputs = if cmd.eval.is_empty() {
            ScriptInputs::Stdin
        } else {
            ScriptInputs::Args(cmd.eval)
        };
        let script = ScriptMode {
            inputs,
            continue_on_error: cmd.continue_on_error,
            req_tx,
            ui_rx,
            next_id,
            completer,
            ui: ui.dupe(),
            outcome_tx,
            shutdown: shutdown.dupe(),
            run_env,
        };
        thread_spawn("repl-editor", move || script.run())
    };
    let thread = match thread {
        Ok(thread) => thread,
        Err(e) => return ExitResult::err(e.into()),
    };

    let mut handler = ReplHandler { ui_tx, compl_tx };
    let result = {
        let mut client = buckd.with_flushing();
        let call = client.repl(
            context,
            build_opts,
            UnboundedReceiverStream::new(req_rx),
            events_ctx,
            &mut handler,
        );
        tokio::select! {
            result = call => Some(result),
            // The call is dropped: the daemon sees the client go away and winds the session
            // down on its own.
            () = sigint => None,
        }
    };
    let reading = ui.end_session();
    // Wakes the input thread if it is waiting for a result.
    let _ignored = handler.ui_tx.send(UiEvent::SessionEnded);

    let Some(result) = result else {
        // Given up on with Ctrl-C. The editor (which is not reading) exits at once; a script
        // may be blocked reading stdin and is left behind.
        if interactive {
            let _ignored = thread.join();
        }
        return ExitResult::signal_interrupt();
    };

    // The input thread sends its outcome before it hangs up, so a session that ended because
    // the inputs ran out always has one. Without one, the session ended early.
    let mut outcome = outcome_rx.try_recv().ok();
    if outcome.is_some() {
        let _ignored = thread.join();
    } else if interactive {
        // The editor restores the terminal before it exits, which it can only do once its
        // current line is read. (Not through rustyline's `ExternalPrinter`: while one exists,
        // rustyline 18 waits on the terminal even when typed-ahead keys are buffered.)
        if reading {
            let message = match shutdown.reason() {
                Some(reason) => {
                    format!("the buck2 daemon was shut down ({reason}); press Enter to exit")
                }
                None => "daemon connection lost; press Enter to exit".to_owned(),
            };
            // The terminal is in raw mode: end the lines explicitly.
            let _ignored = buck2_client_ctx::eprint!("\r\n{}\r\n", message);
        }
        let _ignored = thread.join();
        outcome = outcome_rx.try_recv().ok();
    } // Otherwise a script may be blocked reading stdin: it is left behind.

    match result {
        Err(e) => ExitResult::err(e),
        Ok(CommandOutcome::Failure(exit)) => exit,
        Ok(CommandOutcome::Success(_)) => match (outcome, shutdown.reason()) {
            (Some(outcome), Some(reason)) if outcome.ended_by_shutdown() => {
                daemon_shutdown_error(&reason)
            }
            (Some(outcome), _) => outcome.exit_result(),
            (None, Some(reason)) => daemon_shutdown_error(&reason),
            (None, None) => ExitResult::err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "the daemon ended the repl session before all inputs were evaluated"
            )),
        },
    }
}

fn daemon_shutdown_error(reason: &str) -> ExitResult {
    ExitResult::err(buck2_error::buck2_error!(
        buck2_error::ErrorTag::InterruptedByDaemonShutdown,
        "the buck2 daemon was shut down, which ended the repl session: {reason}"
    ))
}
