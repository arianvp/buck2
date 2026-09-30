/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! The session: the daemon call on the client's runtime, and the thread that reads inputs.
//!
//! The input thread sends requests through `req_tx` (the request stream of the call) and
//! waits for their results on `ui_rx`, which [`ReplHandler`] feeds from the call's partial
//! results. Output (`ReplOutput`) is written by the handler as it arrives, so it is always
//! printed before the `ReplDone` of its request is rendered.

use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use buck2_cli_proto::ClientContext;
use buck2_cli_proto::ReplCompletions;
use buck2_cli_proto::ReplDone;
use buck2_cli_proto::ReplHangup;
use buck2_cli_proto::ReplMessage;
use buck2_cli_proto::ReplNotice;
use buck2_cli_proto::ReplOpen;
use buck2_cli_proto::ReplOutput;
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
use buck2_events::BuckEvent;
use buck2_util::threads::thread_spawn;
use dupe::Dupe;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::ReplCommand;
use crate::script::ScriptInputs;
use crate::script::ScriptMode;
use crate::script::ScriptOutcome;

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
    /// The answer to `Open`. (The interactive editor will need its `ReplReady` for the banner
    /// and the prompt.)
    Ready,
    Done(u64, ReplDone),
    Notice(ReplNotice),
    /// The daemon call is over: nothing else will arrive.
    SessionEnded,
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
            Some(repl_message::Message::Ready(_)) => {
                let _ignored = self.ui_tx.send(UiEvent::Ready);
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

pub(crate) async fn run(
    cmd: ReplCommand,
    context: ClientContext,
    buckd: &mut BuckdClientConnector,
    events_ctx: &mut EventsCtx,
) -> ExitResult {
    let build_opts = cmd.build_opts.to_proto();
    let (req_tx, req_rx) = tokio::sync::mpsc::unbounded_channel::<ReplRequest>();
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();
    let (compl_tx, compl_rx) = std::sync::mpsc::channel::<(u64, ReplCompletions)>();
    let (outcome_tx, mut outcome_rx) = tokio::sync::oneshot::channel::<ScriptOutcome>();
    let next_id = Arc::new(AtomicU64::new(OPEN_ID + 1));
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
        outcome_tx,
        shutdown: shutdown.dupe(),
    };
    let thread = match thread_spawn("repl-editor", move || script.run()) {
        Ok(thread) => thread,
        Err(e) => return ExitResult::err(e.into()),
    };

    let mut handler = ReplHandler { ui_tx, compl_tx };
    let result = buckd
        .with_flushing()
        .repl(
            context,
            build_opts,
            UnboundedReceiverStream::new(req_rx),
            events_ctx,
            &mut handler,
        )
        .await;
    // Wakes the input thread if it is still waiting for a result.
    let _ignored = handler.ui_tx.send(UiEvent::SessionEnded);
    drop(compl_rx);

    // The input thread sends its outcome before it hangs up, so a session that ended because
    // the inputs ran out always has one. Without one, the session ended early; the input thread
    // may be blocked reading stdin and is left behind.
    let outcome = outcome_rx.try_recv().ok();
    if outcome.is_some() {
        let _ignored = thread.join();
    }
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
