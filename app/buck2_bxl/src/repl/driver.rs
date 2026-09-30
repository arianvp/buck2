/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! The request loop of a session.
//!
//! One request at a time uses DICE (INV-14); requests that arrive meanwhile get `BUSY`, except
//! `Interrupt` and `Hangup`. A request takes its own transaction and holds nothing afterwards
//! (INV-11). A failing request is answered and the session goes on (INV-10): only the client
//! going away (EOF, an error, `Hangup`), the end of the command, or the death of the session
//! thread end it.

use std::collections::VecDeque;
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use buck2_cli_proto::ReplCompletions;
use buck2_cli_proto::ReplDone;
use buck2_cli_proto::ReplError;
use buck2_cli_proto::ReplReady;
use buck2_cli_proto::ReplRequest;
use buck2_cli_proto::ReplResponse;
use buck2_cli_proto::ReplValue;
use buck2_cli_proto::TargetCfg;
use buck2_cli_proto::repl_completions;
use buck2_cli_proto::repl_done;
use buck2_cli_proto::repl_error;
use buck2_cli_proto::repl_message;
use buck2_cli_proto::repl_output;
use buck2_cli_proto::repl_request;
use buck2_common::dice::cells::HasCellResolver;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::target::label::label::TargetLabel;
use buck2_error::buck2_error;
use buck2_events::dispatch::current_span;
use buck2_repl_syntax::commands::Handler;
use buck2_repl_syntax::commands::parse_command;
use buck2_server_ctx::ctx::ServerCommandContextTrait;
use buck2_server_ctx::ctx::ServerCommandDiceContext;
use buck2_server_ctx::global_cfg_options::global_cfg_options_from_client_context;
use buck2_server_ctx::streaming_request_handler::StreamingRequestHandler;
use dice::DiceEquality;
use dice_futures::cancellation::CancellationContext;
use dice_futures::cancellation::CancellationObserver;
use dice_futures::spawn::prepare_detached_cancellation;
use dupe::Dupe;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::Fuse;
use tokio::runtime::Handle;

use crate::repl::cancel::EvalCancel;
use crate::repl::line_ctx::ReplCtx;
use crate::repl::output::ReplEmitter;
use crate::repl::render::ReplFailure;
use crate::repl::thread::EvalJob;
use crate::repl::thread::EvalReply;
use crate::repl::thread::EvalWork;
use crate::repl::thread::Job;
use crate::repl::thread::ReplThread;
use crate::repl::thread::SessionConfig;

/// The heap limit when `ReplOpen` does not set one.
const DEFAULT_HEAP_LIMIT: u64 = 4 << 30;

/// A request, classified.
enum Work {
    /// Start the session: import the prelude, bind `ctx`. Answered with `Ready`.
    Init {
        id: u64,
    },
    Eval(EvalRequest),
    /// Answered at once, without DICE.
    Reply {
        id: u64,
        message: repl_message::Message,
    },
    /// Nothing is in flight, so there is nothing to interrupt.
    Interrupt,
    Hangup,
}

struct EvalRequest {
    id: u64,
    number: u32,
    input: String,
}

/// The result of a request that ran in a transaction.
enum Outcome {
    /// Cancelled before it started (e.g. while waiting for other commands).
    Interrupted { t0: Instant, t1: Instant },
    /// The session thread is gone, which ends the session.
    ThreadExited,
    Eval {
        reply: Box<EvalReply>,
        cwd: CellPath,
        target_platform: Option<TargetLabel>,
        equality: DiceEquality,
        t0: Instant,
        t1: Instant,
        t2: Instant,
    },
}

/// What happened while a request was in flight.
enum Event {
    SessionEnded,
    Request(Option<buck2_error::Result<ReplRequest>>),
}

pub(crate) struct Driver<'a> {
    sctx: &'a dyn ServerCommandContextTrait,
    req: StreamingRequestHandler<ReplRequest>,
    /// Fires when the command is cancelled (e.g. the client disconnected). Fused: it is polled
    /// again after it fires (INV-15).
    session_obs: Pin<Box<Fuse<CancellationObserver>>>,
    emitter: ReplEmitter,
    /// The request stream may be polled (INV-15). Cleared when the session is over.
    client_open: bool,
    /// The DICE version of the previous request, to notice source changes.
    last_equality: Option<DiceEquality>,
}

impl<'a> Driver<'a> {
    pub(crate) fn new(
        sctx: &'a dyn ServerCommandContextTrait,
        req: StreamingRequestHandler<ReplRequest>,
        session_obs: CancellationObserver,
        emitter: ReplEmitter,
    ) -> Self {
        Driver {
            sctx,
            req,
            session_obs: Box::pin(session_obs.fuse()),
            emitter,
            client_open: true,
            last_equality: None,
        }
    }

    pub(crate) async fn run(mut self) -> buck2_error::Result<ReplResponse> {
        let Some(first) = self.next_request().await else {
            return Ok(ReplResponse {});
        };
        let (open_id, open) = match first {
            ReplRequest {
                id,
                request: Some(repl_request::Request::Open(open)),
            } => (id, open),
            _ => {
                return Err(buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "the first request of a repl session must be `Open`"
                ));
            }
        };
        let heap_limit = match open.max_heap_bytes {
            0 => DEFAULT_HEAP_LIMIT,
            n => n,
        };
        let cfg = SessionConfig {
            heap_limit: usize::try_from(heap_limit).unwrap_or(usize::MAX),
        };
        let target_cfg = open.target_cfg.unwrap_or_default();
        let thread = ReplThread::spawn(Handle::current(), cfg, self.emitter.dupe())?;

        let mut pending = VecDeque::from([Work::Init { id: open_id }]);
        loop {
            let work = match pending.pop_front() {
                Some(work) => work,
                None => match self.next_request().await {
                    Some(request) => classify(request),
                    None => break,
                },
            };
            match work {
                Work::Hangup => break,
                Work::Interrupt => {}
                Work::Reply { id, message } => self.emitter.emit(id, message),
                Work::Init { id } => {
                    let outcome = self
                        .in_flight(&thread, &target_cfg, id, 0, String::new())
                        .await;
                    self.answer_init(id, outcome);
                }
                Work::Eval(EvalRequest { id, number, input }) => {
                    let outcome = self
                        .in_flight(&thread, &target_cfg, id, number, input)
                        .await;
                    self.answer_eval(id, outcome);
                }
            }
            if !self.client_open {
                break;
            }
        }
        // No job is in flight: every one is awaited to its end.
        thread.shutdown().await;
        Ok(ReplResponse {})
    }

    /// The next request while idle. `None` when the session is over.
    async fn next_request(&mut self) -> Option<ReplRequest> {
        if !self.client_open {
            return None;
        }
        let event = tokio::select! {
            _ = &mut self.session_obs => Event::SessionEnded,
            m = self.req.next() => Event::Request(m),
        };
        match event {
            Event::Request(Some(Ok(request))) => Some(request),
            // The client is gone (EOF, or an error of the stream), or the command is over.
            Event::Request(None | Some(Err(_))) | Event::SessionEnded => {
                self.client_open = false;
                None
            }
        }
    }

    /// Runs an evaluation to its end while answering the requests that arrive meanwhile.
    async fn in_flight(
        &mut self,
        thread: &ReplThread,
        target_cfg: &TargetCfg,
        id: u64,
        number: u32,
        input: String,
    ) -> buck2_error::Result<Outcome> {
        let cancel = Arc::new(EvalCancel::new());
        let fut = run_eval(
            self.sctx,
            thread.jobs(),
            target_cfg.clone(),
            EvalRequest { id, number, input },
            cancel.dupe(),
            self.emitter.dupe(),
        );
        tokio::pin!(fut);
        loop {
            // `fut` is polled until it completes, never dropped (INV-8).
            let event = tokio::select! {
                biased;
                outcome = &mut fut => return outcome,
                _ = &mut self.session_obs, if self.client_open => Event::SessionEnded,
                m = self.req.next(), if self.client_open => Event::Request(m),
            };
            match event {
                Event::SessionEnded | Event::Request(None | Some(Err(_))) => {
                    cancel.trigger();
                    self.client_open = false;
                }
                Event::Request(Some(Ok(request))) => match request.request {
                    Some(repl_request::Request::Hangup(_)) => {
                        cancel.trigger();
                        self.client_open = false;
                    }
                    Some(repl_request::Request::Interrupt(interrupt)) => {
                        if interrupt.target_id == id {
                            cancel.trigger();
                        }
                    }
                    Some(repl_request::Request::Complete(_)) => self.emitter.emit(
                        request.id,
                        repl_message::Message::Completions(ReplCompletions {
                            status: repl_completions::Status::Busy as i32,
                            candidates: Vec::new(),
                            message: String::new(),
                        }),
                    ),
                    // One request at a time (INV-14).
                    Some(repl_request::Request::Eval(_) | repl_request::Request::Open(_))
                    | None => self.emitter.done(
                        request.id,
                        error_done(
                            repl_error::Kind::Busy,
                            &"busy: another request of this session is running",
                        ),
                    ),
                },
            }
        }
    }

    fn answer_init(&mut self, id: u64, outcome: buck2_error::Result<Outcome>) {
        if let Ok(Outcome::Eval {
            reply,
            cwd,
            target_platform,
            equality,
            ..
        }) = &outcome
            && reply.result.is_ok()
        {
            self.last_equality = Some(*equality);
            self.emitter.emit(
                id,
                repl_message::Message::Ready(ReplReady {
                    cwd: cwd.to_string(),
                    target_platform: target_platform
                        .as_ref()
                        .map(|t| t.to_string())
                        .unwrap_or_default(),
                    prelude_loaded: reply.prelude_loaded,
                }),
            );
            return;
        }
        // The session could not start: answer `Open` with the error.
        self.answer_eval(id, outcome)
    }

    fn answer_eval(&mut self, id: u64, outcome: buck2_error::Result<Outcome>) {
        let done = match outcome {
            Err(e) => ReplDone {
                outcome: Some(repl_done::Outcome::Error(failure_proto(
                    ReplFailure::from_buck2(repl_error::Kind::Buck, &e),
                ))),
                ..ReplDone::default()
            },
            Ok(Outcome::ThreadExited) => {
                // Nothing can be evaluated any more.
                self.client_open = false;
                error_done(
                    repl_error::Kind::Internal,
                    &"the repl session thread exited; the session is over",
                )
            }
            Ok(Outcome::Interrupted { t0, t1 }) => ReplDone {
                outcome: Some(repl_done::Outcome::Error(failure_proto(
                    ReplFailure::interrupted(),
                ))),
                wait_ms: millis(t1 - t0),
                ..ReplDone::default()
            },
            Ok(Outcome::Eval {
                reply,
                equality,
                t0,
                t1,
                t2,
                ..
            }) => {
                let sources_changed = self.last_equality.is_some_and(|last| last != equality);
                self.last_equality = Some(equality);
                let EvalReply {
                    result,
                    drained,
                    heap_bytes,
                    prelude_loaded: _,
                } = *reply;
                let outcome = match (result, drained) {
                    (Err(failure), _) => Some(repl_done::Outcome::Error(failure_proto(failure))),
                    (Ok(_), Err(e)) => Some(repl_done::Outcome::Error(failure_proto(
                        ReplFailure::from_buck2(repl_error::Kind::Buck, &e),
                    ))),
                    (Ok(None), Ok(_)) => None,
                    (Ok(Some(value)), Ok(_)) => Some(repl_done::Outcome::Value(ReplValue {
                        r#type: value.type_name,
                        text: value.text,
                        truncated: value.truncated,
                        json: None,
                    })),
                };
                ReplDone {
                    outcome,
                    wait_ms: millis(t1 - t0),
                    eval_ms: millis(t2 - t1),
                    sources_changed,
                    heap_bytes,
                }
            }
        };
        self.emitter.done(id, done);
    }
}

/// Classifies a request that arrived while nothing was in flight.
fn classify(request: ReplRequest) -> Work {
    let id = request.id;
    let Some(request) = request.request else {
        return Work::Reply {
            id,
            message: repl_message::Message::Done(error_done(
                repl_error::Kind::Internal,
                &"malformed repl request (no request)",
            )),
        };
    };
    match request {
        repl_request::Request::Open(_) => Work::Reply {
            id,
            message: repl_message::Message::Done(error_done(
                repl_error::Kind::Usage,
                &"the repl session is already open",
            )),
        },
        repl_request::Request::Eval(eval) => match parse_command(&eval.input) {
            Ok(None) => Work::Eval(EvalRequest {
                id,
                number: eval.number,
                input: eval.input,
            }),
            Ok(Some(command)) => {
                let name = command.spec.display_name();
                let done = match command.spec.handler {
                    Handler::Client => error_done(
                        repl_error::Kind::Usage,
                        &format_args!("`{name}` is handled by the client"),
                    ),
                    Handler::Server | Handler::Both => error_done(
                        repl_error::Kind::Unsupported,
                        &format_args!("`{name}` is not implemented yet"),
                    ),
                };
                Work::Reply {
                    id,
                    message: repl_message::Message::Done(done),
                }
            }
            // The message quotes the token, which may be of any length.
            Err(e) => Work::Reply {
                id,
                message: repl_message::Message::Done(error_done(repl_error::Kind::Usage, &e)),
            },
        },
        // Completion comes later: no candidates.
        repl_request::Request::Complete(_) => Work::Reply {
            id,
            message: repl_message::Message::Completions(ReplCompletions {
                status: repl_completions::Status::Ok as i32,
                candidates: Vec::new(),
                message: String::new(),
            }),
        },
        repl_request::Request::Interrupt(_) => Work::Interrupt,
        repl_request::Request::Hangup(_) => Work::Hangup,
    }
}

/// Evaluates an input in a transaction of its own: the only place an evaluation takes one.
async fn run_eval(
    sctx: &dyn ServerCommandContextTrait,
    jobs: std::sync::mpsc::Sender<Job>,
    target_cfg: TargetCfg,
    request: EvalRequest,
    cancel: Arc<EvalCancel>,
    emitter: ReplEmitter,
) -> buck2_error::Result<Outcome> {
    let EvalRequest { id, number, input } = request;
    let t0 = Instant::now();
    let repl_ctx = ReplCtx::eval(sctx, &input);
    (&repl_ctx as &dyn ServerCommandContextTrait)
        .with_dice_ctx(|sctx, txn| async move {
            let t1 = Instant::now();
            if cancel.is_triggered() {
                // Cancelled while waiting for other commands.
                return Ok(Outcome::Interrupted { t0, t1 });
            }
            let (cwd, global_cfg_options) = {
                let mut dc = txn.ctx();
                let cwd = dc
                    .get_cell_resolver()
                    .await?
                    .get_cell_path(sctx.working_dir());
                let global_cfg_options =
                    global_cfg_options_from_client_context(&target_cfg, sctx, &mut dc).await?;
                (cwd, global_cfg_options)
            };
            let target_platform = global_cfg_options.target_platform.clone();

            // The job runs in a task of its own, whose structured cancellation the session
            // thread observes; an interrupt cancels that task, never this future (INV-8).
            let slot: Arc<Mutex<Option<EvalReply>>> = Arc::new(Mutex::new(None));
            let (spawner, handle) = prepare_detached_cancellation();
            cancel.attach(handle);
            let data = txn.per_transaction_data();
            let job_slot = slot.dupe();
            let job_txn = txn.dupe();
            let job_cwd = cwd.clone();
            let span = current_span();
            let join = spawner.spawn(
                move |cc: &CancellationContext| {
                    async move {
                        cc.with_structured_cancellation(move |liveness| async move {
                            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                            let job = Job::Eval(EvalJob {
                                work: EvalWork {
                                    id,
                                    number,
                                    input,
                                    txn: job_txn,
                                    // A fresh observer for every job (INV-6).
                                    liveness,
                                    cwd: job_cwd,
                                    global_cfg_options,
                                    span,
                                },
                                reply: reply_tx,
                            });
                            if jobs.send(job).is_ok() {
                                // The thread has dropped every DICE handle of the job when it
                                // replies (INV-4).
                                if let Ok(reply) = reply_rx.await {
                                    *job_slot.lock().unwrap_or_else(PoisonError::into_inner) =
                                        Some(reply);
                                }
                            }
                        })
                        .await
                    }
                    .boxed()
                },
                &*data.spawner,
                data,
            );
            // Finished or cancelled: either way, the thread has replied, or never got the job.
            let _ignored = join.await;
            let reply = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
            let Some(reply) = reply else {
                return Ok(if cancel.is_triggered() {
                    Outcome::Interrupted {
                        t0,
                        t1: Instant::now(),
                    }
                } else {
                    Outcome::ThreadExited
                });
            };

            // `ctx.output.print` output, after the evaluation as in `buck2 bxl`. Streamed output
            // was shown as it was written.
            if let Ok(drained) = &reply.drained {
                emitter.output(id, repl_output::Channel::Stdout, &drained.output);
                emitter.output(id, repl_output::Channel::Stderr, &drained.error);
            }

            Ok(Outcome::Eval {
                reply: Box::new(reply),
                cwd,
                target_platform,
                equality: txn.equality_token(),
                t0,
                t1,
                t2: Instant::now(),
            })
        })
        .await
}

/// A `Done` with an error, whose message is capped like every other (INV-13).
fn error_done(kind: repl_error::Kind, message: &dyn fmt::Display) -> ReplDone {
    ReplDone {
        outcome: Some(repl_done::Outcome::Error(failure_proto(ReplFailure::new(
            kind, message,
        )))),
        ..ReplDone::default()
    }
}

fn failure_proto(failure: ReplFailure) -> ReplError {
    ReplError {
        kind: failure.kind as i32,
        message: failure.message,
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}
