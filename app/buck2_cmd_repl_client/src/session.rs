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
//! interactive [`editor`](crate::editor) or the [`script`](crate::script) reader, which evaluate
//! them with [`Inputs`]), and the SIGINT handler.
//!
//! The input thread sends requests through `req_tx` (the request stream of the call) and
//! waits for their results on `ui_rx`, which [`ReplHandler`] feeds from the call's partial
//! results. Output (`ReplOutput`) is written by the handler as it arrives, so it is always
//! printed before the `ReplDone` of its request is rendered. The handler writes it through the
//! [`ReplConsole`], which also shows the live progress of inputs (and erases it before the input
//! thread prints anything of an input).

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
use buck2_cli_proto::ClientContext;
use buck2_cli_proto::ReplCompletions;
use buck2_cli_proto::ReplDone;
use buck2_cli_proto::ReplHangup;
use buck2_cli_proto::ReplInterrupt;
use buck2_cli_proto::ReplMessage;
use buck2_cli_proto::ReplNotice;
use buck2_cli_proto::ReplOpen;
use buck2_cli_proto::ReplReady;
use buck2_cli_proto::ReplRequest;
use buck2_cli_proto::repl_message;
use buck2_cli_proto::repl_request;
use buck2_client_ctx::command_outcome::CommandOutcome;
use buck2_client_ctx::daemon::client::BuckdClientConnector;
use buck2_client_ctx::events_ctx::EventsCtx;
use buck2_client_ctx::events_ctx::PartialResultCtx;
use buck2_client_ctx::events_ctx::PartialResultHandler;
use buck2_client_ctx::exit_result::ExitResult;
use buck2_client_ctx::subscribers::subscriber::EventSubscriber;
use buck2_error::ExitCode;
use buck2_event_observer::verbosity::Verbosity;
use buck2_events::BuckEvent;
use buck2_util::threads::thread_spawn;
use buck2_wrapper_common::invocation_id::TraceId;
use dupe::Dupe;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::ConsoleMode;
use crate::ReplCommand;
use crate::complete::Completer;
use crate::console::ReplConsole;
use crate::console::Setup;
use crate::editor::EditorMode;
use crate::editor::history_path;
use crate::inputs::FirstInputs;
use crate::inputs::Inputs;
use crate::inputs::Mode;
use crate::inputs::SessionIo;
use crate::json::JsonCapture;
use crate::run::RunEnv;
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
    /// A script waits for its next line of stdin, which may never come.
    reading_stdin: bool,
    /// The daemon call is over: the input thread must not wait for input any more.
    session_ended: bool,
}

/// What the input thread does when the session ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AtEnd {
    /// The line editor reads a line: it only notices the end after it.
    EditorReading,
    /// A script waits for its next line of stdin, which may never come. Every input it read
    /// before is done (and reported); it stops without evaluating the line it gets.
    StdinReading,
    /// Anything else, e.g. waiting for the result of an input: the thread notices the end at
    /// once (or when the program that it runs, `:!`, is over) and reports the input.
    Running,
}

impl SharedUi {
    fn new() -> Self {
        SharedUi(Arc::new(Mutex::new(UiInner {
            state: UiState::Idle,
            reading: false,
            reading_stdin: false,
            session_ended: false,
        })))
    }

    fn lock(&self) -> MutexGuard<'_, UiInner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn set(&self, state: UiState) {
        self.lock().state = state;
    }

    /// The id of the request that runs, if any.
    pub(crate) fn busy(&self) -> Option<u64> {
        match self.lock().state {
            UiState::Busy { id, .. } => Some(id),
            UiState::Idle | UiState::Editor | UiState::Child => None,
        }
    }

    /// Whether the terminal is someone else's: the line editor reads a line, or a program that
    /// the session runs (`:run`, `:!`, `:edit`) runs.
    pub(crate) fn terminal_taken(&self) -> bool {
        let inner = self.lock();
        inner.reading || inner.state == UiState::Child
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

    /// A script starts waiting for its next line of stdin, unless the session is over. Returns
    /// whether it is not (so that the script may read).
    pub(crate) fn start_reading_stdin(&self) -> bool {
        let mut inner = self.lock();
        if inner.session_ended {
            return false;
        }
        inner.reading_stdin = true;
        true
    }

    /// A script got its next line of stdin (or the end of stdin). Returns whether the session
    /// is still on. If it ended meanwhile, the session did not wait for the input thread (see
    /// [`AtEnd::StdinReading`]), which must not start anything more.
    pub(crate) fn stop_reading_stdin(&self) -> bool {
        let mut inner = self.lock();
        inner.reading_stdin = false;
        !inner.session_ended
    }

    /// Marks the session as over. Returns what the input thread is doing.
    fn end_session(&self) -> AtEnd {
        let mut inner = self.lock();
        inner.session_ended = true;
        if inner.reading {
            AtEnd::EditorReading
        } else if inner.reading_stdin {
            AtEnd::StdinReading
        } else {
            AtEnd::Running
        }
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
    /// Writes the output, and erases the live progress of an input before the input thread
    /// prints something of it.
    console: ReplConsole,
    /// With `--json`, the output goes into the record of its input instead.
    json: Option<JsonCapture>,
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
            Some(repl_message::Message::Output(output)) => match &self.json {
                Some(capture) => capture.write(output.channel(), &output.data),
                None => self.console.write_output(&output).await?,
            },
            Some(repl_message::Message::Ready(ready)) => {
                let _ignored = self.ui_tx.send(UiEvent::Ready(ready));
            }
            Some(repl_message::Message::Done(done)) => {
                self.console.before_done(id).await?;
                let _ignored = self.ui_tx.send(UiEvent::Done(id, done));
            }
            Some(repl_message::Message::Notice(notice)) => {
                self.console.before_notice().await?;
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

/// Handles SIGINT (Ctrl-C) while the session runs, according to the [`UiState`]. Returns when
/// the client should give up on the session and exit.
async fn sigint_loop(
    ui: SharedUi,
    req_tx: tokio::sync::mpsc::WeakUnboundedSender<ReplRequest>,
    next_id: Arc<AtomicU64>,
    console: ReplConsole,
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
        // The lock is released before the messages are printed.
        let presses = {
            let mut inner = ui.lock();
            match &mut inner.state {
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
            }
        };
        // Output errors do not matter here: the input thread reports them.
        match presses {
            1 => {
                let _ignored = console.message(&format!("{prefix}^C interrupting…")).await;
            }
            2 => {
                let _ignored = console
                    .message(&format!(
                        "{prefix}^C still cancelling — the daemon may be waiting for another \
                         buck2 command; ^C again to quit"
                    ))
                    .await;
            }
            _ => return,
        }
    }
}

pub(crate) async fn run(
    cmd: ReplCommand,
    context: ClientContext,
    trace_id: TraceId,
    verbosity: Verbosity,
    buckd: &mut BuckdClientConnector,
    events_ctx: &mut EventsCtx,
) -> ExitResult {
    let read_stdin = cmd.reads_stdin();
    let interactive = cmd.is_interactive();
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
    let console = cmd.console.dupe();
    if let ConsoleMode::Live { forced } = cmd.console_mode() {
        console
            .start(Setup {
                trace_id,
                verbosity,
                config: cmd.common_opts.console_opts.superconsole_config(),
                forced,
                ui: ui.dupe(),
            })
            .await;
    }

    let json = cmd.json.then(|| cmd.json_capture.dupe());
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
    let sigint = sigint_loop(
        ui.dupe(),
        req_tx.downgrade(),
        next_id.dupe(),
        console.dupe(),
    );
    let io = SessionIo {
        req_tx,
        ui_rx,
        next_id,
        completer,
        ui: ui.dupe(),
        shutdown: shutdown.dupe(),
        run_env,
        json: json.dupe(),
    };
    let first = FirstInputs {
        files: cmd.files,
        evals: cmd.eval,
    };
    let thread = if interactive {
        let editor = EditorMode {
            inputs: Inputs::new(io, Mode::Interactive),
            first,
            history,
            outcome_tx,
        };
        thread_spawn("repl-editor", move || editor.run())
    } else {
        let mode = Mode::Script {
            continue_on_error: cmd.continue_on_error,
        };
        let script = ScriptMode {
            inputs: Inputs::new(io, mode),
            first,
            read_stdin,
            outcome_tx,
        };
        thread_spawn("repl-editor", move || script.run())
    };
    let thread = match thread {
        Ok(thread) => thread,
        Err(e) => return ExitResult::err(e.into()),
    };

    let mut handler = ReplHandler {
        ui_tx,
        compl_tx,
        console: console.dupe(),
        json,
    };
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
    // Nothing is drawn after the call: the terminal is the input thread's (and the messages
    // below are printed after the progress of an input that was cut off).
    let _ignored = console.end().await;
    let at_end = ui.end_session();
    // Wakes the input thread if it is waiting for a result.
    let _ignored = handler.ui_tx.send(UiEvent::SessionEnded);

    let Some(result) = result else {
        // Given up on with Ctrl-C. The editor (which is not reading) exits at once. A script
        // that runs an input reports it (with `--json`, its record); one that waits for stdin
        // is left behind.
        let stopped = interactive
            || (at_end == AtEnd::Running && script_outcome(&mut outcome_rx).await.is_some());
        if stopped {
            let _ignored = thread.join();
        }
        return ExitResult::signal_interrupt();
    };

    // The input thread sends its outcome before it hangs up, so a session that ended because
    // the inputs ran out always has one. Without one, the session ended early.
    let mut outcome = outcome_rx.try_recv().ok();
    if outcome.is_none() && !interactive && at_end == AtEnd::Running {
        // The script runs an input, whose result will not come: it reports it (with `--json`,
        // the input gets its record) and stops.
        outcome = script_outcome(&mut outcome_rx).await;
    }
    if outcome.is_some() {
        let _ignored = thread.join();
    } else if interactive {
        // The editor restores the terminal before it exits, which it can only do once its
        // current line is read. (Not through rustyline's `ExternalPrinter`: while one exists,
        // rustyline 18 waits on the terminal even when typed-ahead keys are buffered.)
        if at_end == AtEnd::EditorReading {
            let message = match shutdown.reason() {
                Some(reason) => {
                    format!("the buck2 daemon was shut down ({reason}); press Enter to exit")
                }
                None => "daemon connection lost; press Enter to exit".to_owned(),
            };
            // The terminal is in raw mode: end the lines explicitly. What the editor shows
            // under the input (a signature hint) is erased.
            let erase = if std::io::stderr().is_terminal() {
                "\x1b[J"
            } else {
                ""
            };
            let _ignored = buck2_client_ctx::eprint!("\r\n{}{}\r\n", erase, message);
        }
        let _ignored = thread.join();
        outcome = outcome_rx.try_recv().ok();
    } // Otherwise a script waits for stdin (or took too long to stop): it is left behind.

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

/// How long a script that runs an input when the session ends is given to report it. It is
/// quick unless a program that the input runs (`:!`) goes on, or nobody reads stdout.
const SCRIPT_WIND_DOWN: Duration = Duration::from_secs(10);

/// The outcome of a script that ran an input when the session ended, once it has reported it.
async fn script_outcome(
    outcome_rx: &mut tokio::sync::oneshot::Receiver<InputOutcome>,
) -> Option<InputOutcome> {
    tokio::time::timeout(SCRIPT_WIND_DOWN, outcome_rx)
        .await
        .ok()?
        .ok()
}

fn daemon_shutdown_error(reason: &str) -> ExitResult {
    ExitResult::err(buck2_error::buck2_error!(
        buck2_error::ErrorTag::InterruptedByDaemonShutdown,
        "the buck2 daemon was shut down, which ended the repl session: {reason}"
    ))
}
