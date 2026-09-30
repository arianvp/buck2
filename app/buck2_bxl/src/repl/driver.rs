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
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use buck2_build_api::bxl::result::PendingStreamingOutput;
use buck2_build_api::materialize::MaterializationAndUploadContext;
use buck2_cli_proto::ReplComplete;
use buck2_cli_proto::ReplCompletions;
use buck2_cli_proto::ReplDone;
use buck2_cli_proto::ReplError;
use buck2_cli_proto::ReplReady;
use buck2_cli_proto::ReplRequest;
use buck2_cli_proto::ReplResponse;
use buck2_cli_proto::ReplValue;
use buck2_cli_proto::TargetCfg;
use buck2_cli_proto::repl_complete;
use buck2_cli_proto::repl_completions;
use buck2_cli_proto::repl_done;
use buck2_cli_proto::repl_error;
use buck2_cli_proto::repl_message;
use buck2_cli_proto::repl_notice;
use buck2_cli_proto::repl_output;
use buck2_cli_proto::repl_request;
use buck2_common::dice::cells::HasCellResolver;
use buck2_common::events::HasEvents;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::target::label::label::TargetLabel;
use buck2_data::BxlEnsureArtifactsEnd;
use buck2_data::BxlEnsureArtifactsStart;
use buck2_error::buck2_error;
use buck2_events::dispatch::current_span;
use buck2_repl_syntax::commands::parse_command;
use buck2_repl_syntax::text::truncate_to_bytes;
use buck2_server_ctx::ctx::ServerCommandContextTrait;
use buck2_server_ctx::ctx::ServerCommandDiceContext;
use buck2_server_ctx::global_cfg_options::global_cfg_options_from_client_context;
use buck2_server_ctx::streaming_request_handler::StreamingRequestHandler;
use dice::DiceEquality;
use dice::DiceTransaction;
use dice_futures::cancellation::CancellationContext;
use dice_futures::cancellation::CancellationObserver;
use dice_futures::spawn::prepare_detached_cancellation;
use dupe::Dupe;
use dupe::IterDupedExt;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::Fuse;
use tokio::runtime::Handle;

use crate::bxl::starlark_defs::context::output::OutputStreamOutcome;
use crate::command::materialize_ensured_artifacts;
use crate::repl::build::BuildSpec;
use crate::repl::build::Built;
use crate::repl::build::build;
use crate::repl::bxl::BxlSpec;
use crate::repl::bxl::complete_bxl_functions;
use crate::repl::bxl::run_bxl;
use crate::repl::cancel::EvalCancel;
use crate::repl::commands::CommandWork;
use crate::repl::commands::command_work;
use crate::repl::complete::candidates::completions_status;
use crate::repl::complete::loads::complete_load_symbols;
use crate::repl::complete::query::query_functions;
use crate::repl::complete::targets::complete_load_paths;
use crate::repl::complete::targets::complete_targets;
use crate::repl::inspect::InspectSpec;
use crate::repl::inspect::Inspected;
use crate::repl::inspect::inspect;
use crate::repl::line_ctx::ReplCtx;
use crate::repl::output::ReplEmitter;
use crate::repl::output::ReplOutputWriter;
use crate::repl::render::MAX_JSON_BYTES;
use crate::repl::render::Rendered;
use crate::repl::render::ReplFailure;
use crate::repl::settings::SetWork;
use crate::repl::settings::setting_changed;
use crate::repl::settings::show_settings;
use crate::repl::thread::EvalJob;
use crate::repl::thread::EvalKind;
use crate::repl::thread::EvalReply;
use crate::repl::thread::EvalWork;
use crate::repl::thread::Job;
use crate::repl::thread::ReplThread;
use crate::repl::thread::SessionConfig;

/// The heap limit when `ReplOpen` does not set one.
const DEFAULT_HEAP_LIMIT: u64 = 4 << 30;

/// Longest excerpt of an input kept to describe its request to other commands.
const MAX_TITLE_BYTES: usize = 256;

/// Longest a completion from DICE (of a target pattern, a module to load or its symbols, or a
/// BXL function) may take. The client stops waiting sooner; the work goes on meanwhile (unless
/// an input cancels it), so that the next completion is fast.
const DICE_COMPLETION_TIMEOUT: Duration = Duration::from_secs(5);

/// A request, classified.
enum Work {
    /// Start the session: import the prelude, bind `ctx`. Answered with `Ready`.
    Init {
        id: u64,
    },
    Eval(EvalRequest),
    /// `:build`, `:run`.
    Build(BuildRequest),
    /// `:bxl`.
    Bxl(BxlRequest),
    /// `:info`, `:ls`, `:__locate`.
    Inspect(InspectRequest),
    /// `:set`, for the settings of the daemon.
    Set {
        id: u64,
        work: SetWork,
        title: String,
    },
    /// Text made at once, without DICE (`:qdoc`): sent as output.
    Text {
        id: u64,
        text: String,
        format: repl_output::Format,
    },
    /// `:reset`: a new session, started like the first one.
    Reset {
        id: u64,
    },
    /// Completion of a name, an attribute or a keyword argument, by the session thread (no
    /// DICE).
    CompleteStarlark {
        id: u64,
        req: ReplComplete,
    },
    /// Completion from DICE, in a transaction of its own.
    CompleteDice {
        id: u64,
        what: DiceCompletion,
    },
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
    /// The code to evaluate (for a meta-command, generated from it).
    input: String,
    kind: EvalKind,
    /// The first line of the input as typed, shown to other commands that wait for this one.
    title: String,
}

impl EvalRequest {
    /// Starts the session: imports the prelude and binds `ctx`.
    fn init(id: u64) -> Self {
        EvalRequest {
            id,
            number: 0,
            input: String::new(),
            kind: EvalKind::Input,
            title: String::new(),
        }
    }
}

struct BuildRequest {
    id: u64,
    spec: BuildSpec,
    /// The input as typed, shown to other commands that wait for this one.
    title: String,
}

struct BxlRequest {
    id: u64,
    spec: BxlSpec,
    /// The input as typed, shown to other commands that wait for this one.
    title: String,
}

struct InspectRequest {
    id: u64,
    spec: InspectSpec,
    /// The input as typed, shown to other commands that wait for this one.
    title: String,
}

/// What a completion from DICE completes.
enum DiceCompletion {
    /// A target pattern.
    Targets { prefix: String },
    /// A function of a `.bxl` file (`:bxl <file.bxl>:<prefix>`).
    BxlFunctions { module: String, prefix: String },
    /// A module to load.
    LoadPaths { prefix: String },
    /// A symbol of a module to load, but the `used` ones.
    LoadSymbols {
        module: String,
        prefix: String,
        used: Vec<String>,
    },
}

/// The first line of `input`, cut to [`MAX_TITLE_BYTES`].
fn title(input: &str) -> String {
    let first_line = input.trim_start().lines().next().unwrap_or("");
    truncate_to_bytes(first_line, MAX_TITLE_BYTES + 1).to_owned()
}

/// The result of a request that ran in a transaction.
enum Outcome {
    /// Cancelled before it started (e.g. while waiting for other commands).
    Interrupted { t0: Instant, t1: Instant },
    /// The session thread is gone, which ends the session.
    ThreadExited,
    Eval {
        reply: Box<EvalReply>,
        materialized: Materialized,
        cwd: CellPath,
        target_platform: Option<TargetLabel>,
        equality: DiceEquality,
        t0: Instant,
        t1: Instant,
        t2: Instant,
    },
    /// `:build` or `:run`.
    Built {
        result: Result<Built, ReplFailure>,
        equality: DiceEquality,
        t0: Instant,
        t1: Instant,
        t2: Instant,
    },
    /// `:info`, `:ls`, `:__locate`, or the check of a target platform (`:set`).
    Inspected {
        result: Result<Inspected, ReplFailure>,
        equality: DiceEquality,
        t0: Instant,
        t1: Instant,
        t2: Instant,
    },
    /// `:bxl`, which has no value: its output went to the client as it ran.
    Bxl {
        result: Result<(), ReplFailure>,
        equality: DiceEquality,
        t0: Instant,
        t1: Instant,
        t2: Instant,
    },
}

/// How the materialization of the artifacts an input ensured (`ctx.output.ensure`) went.
enum Materialized {
    /// Every artifact was materialized, or there were none.
    Done,
    Failed(Vec<buck2_error::Error>),
    /// The request was cancelled before every artifact was materialized.
    Interrupted,
}

/// What happened while waiting for a request, or while a request was in flight.
enum Event {
    SessionEnded,
    Request(Option<buck2_error::Result<ReplRequest>>),
    /// The session thread answered the completion `id` (`None`: the thread exited first).
    Completed(u64, Option<ReplCompletions>),
}

/// A completion of a name, an attribute or a keyword argument that the session thread is
/// answering. The driver does not wait for it: it goes on reading requests and running the
/// request in flight (a completion from DICE, whose transaction must give way to other
/// commands), and sends the answer when it comes.
struct ThreadCompletion {
    id: u64,
    answer: tokio::sync::oneshot::Receiver<ReplCompletions>,
}

/// The answer to the thread completion in flight; never resolves while there is none.
async fn thread_answer(
    completing: &mut Option<ThreadCompletion>,
) -> (u64, Option<ReplCompletions>) {
    match completing {
        Some(completion) => (completion.id, (&mut completion.answer).await.ok()),
        None => std::future::pending().await,
    }
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
    /// Bytes allocated on the session's heap, as of the last job of the session thread.
    heap_bytes: u64,
    /// An input that arrived while a completion was in flight, which it cancelled: it runs
    /// next (Enter beats Tab).
    queued: Option<ReplRequest>,
    /// The completion of a name, an attribute or a keyword argument the session thread is
    /// answering, if any: at most one at a time.
    completing: Option<ThreadCompletion>,
    /// Values get their JSON form too (`ReplOpen.json_values`).
    json_values: bool,
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
            heap_bytes: 0,
            queued: None,
            completing: None,
            json_values: false,
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
            json_values: open.json_values,
        };
        self.json_values = open.json_values;
        // `:set` changes it.
        let mut target_cfg = open.target_cfg.unwrap_or_default();
        let thread = ReplThread::spawn(Handle::current(), cfg, self.emitter.dupe())?;

        let mut pending = VecDeque::from([Work::Init { id: open_id }]);
        loop {
            let work = match pending.pop_front() {
                Some(work) => work,
                None => match self.queued.take() {
                    Some(request) => classify(request),
                    None => match self.next_request().await {
                        Some(request) => classify(request),
                        None => break,
                    },
                },
            };
            match work {
                Work::Hangup => break,
                Work::Interrupt => {}
                Work::Reply { id, message } => self.emitter.emit(id, message),
                Work::CompleteStarlark { id, req } => {
                    // Answered when the thread is done (`next_request`, `in_flight`).
                    self.start_thread_completion(&thread, id, req);
                }
                Work::CompleteDice { id, what } => {
                    let cancel = Arc::new(EvalCancel::new());
                    let fut = run_dice_completion(self.sctx, &target_cfg, what, cancel.dupe());
                    let answer = self.in_flight(id, &cancel, Some(&thread), fut).await;
                    self.emitter
                        .emit(id, repl_message::Message::Completions(answer));
                }
                Work::Init { id } => {
                    let outcome = self.eval(&thread, &target_cfg, EvalRequest::init(id)).await;
                    self.answer_init(id, outcome);
                }
                Work::Eval(request) => {
                    let id = request.id;
                    let outcome = self.eval(&thread, &target_cfg, request).await;
                    self.answer_eval(id, outcome);
                }
                Work::Build(request) => {
                    let id = request.id;
                    let outcome = self.build(&thread, &target_cfg, request).await;
                    self.answer_eval(id, outcome);
                }
                Work::Bxl(request) => {
                    let id = request.id;
                    let cancel = Arc::new(EvalCancel::new());
                    let fut = run_bxl_request(
                        self.sctx,
                        target_cfg.clone(),
                        request,
                        cancel.dupe(),
                        self.emitter.dupe(),
                    );
                    let outcome = self.in_flight(id, &cancel, None, fut).await;
                    self.answer_eval(id, outcome);
                }
                Work::Inspect(request) => {
                    let id = request.id;
                    let outcome = self.inspect(request).await;
                    self.answer_eval(id, outcome);
                }
                Work::Set { id, work, title } => self.set(id, work, title, &mut target_cfg).await,
                Work::Text { id, text, format } => {
                    self.send_text_as(id, &text, format);
                    self.done_without_value(id);
                }
                Work::Reset { id } => {
                    // The thread is idle: it drops the module at once.
                    let outcome = if thread.reset().await {
                        self.eval(&thread, &target_cfg, EvalRequest::init(id)).await
                    } else {
                        Ok(Outcome::ThreadExited)
                    };
                    self.answer_reset(id, outcome);
                }
            }
            if !self.client_open {
                break;
            }
        }
        // No request is in flight: every one is awaited to its end. The thread finishes a
        // completion it is answering (it runs no code, and is bounded by the size of the values
        // it looks at) before it exits.
        thread.shutdown().await;
        Ok(ReplResponse {})
    }

    /// The next request while idle, answering the thread completion meanwhile. `None` when the
    /// session is over.
    async fn next_request(&mut self) -> Option<ReplRequest> {
        loop {
            if !self.client_open {
                return None;
            }
            let event = tokio::select! {
                _ = &mut self.session_obs => Event::SessionEnded,
                m = self.req.next() => Event::Request(m),
                (id, answer) = thread_answer(&mut self.completing) => Event::Completed(id, answer),
            };
            match event {
                Event::Request(Some(Ok(request))) => return Some(request),
                Event::Completed(id, answer) => self.answer_thread_completion(id, answer),
                // The client is gone (EOF, or an error of the stream), or the command is over.
                Event::Request(None | Some(Err(_))) | Event::SessionEnded => {
                    self.client_open = false;
                    return None;
                }
            }
        }
    }

    /// Sends the completion `id` of a name, an attribute or a keyword argument to the session
    /// thread, unless it is answering one already (then the answer is `BUSY`). The driver does
    /// not wait for the answer.
    fn start_thread_completion(&mut self, thread: &ReplThread, id: u64, req: ReplComplete) {
        if self.completing.is_some() {
            self.emitter.emit(
                id,
                repl_message::Message::Completions(completions_status(
                    repl_completions::Status::Busy,
                    "busy: another completion of this session is running",
                )),
            );
            return;
        }
        match thread.start_complete(req) {
            Some(answer) => self.completing = Some(ThreadCompletion { id, answer }),
            None => self.answer_thread_completion(id, None),
        }
    }

    /// Sends the thread's answer to the completion `id`. `None`: the thread exited, which ends
    /// the session.
    fn answer_thread_completion(&mut self, id: u64, answer: Option<ReplCompletions>) {
        if self.completing.as_ref().is_some_and(|c| c.id == id) {
            self.completing = None;
        }
        let answer = answer.unwrap_or_else(|| {
            self.client_open = false;
            completions_status(
                repl_completions::Status::Error,
                "the repl session thread exited; the session is over",
            )
        });
        self.emitter
            .emit(id, repl_message::Message::Completions(answer));
    }

    /// Evaluates an input.
    async fn eval(
        &mut self,
        thread: &ReplThread,
        target_cfg: &TargetCfg,
        request: EvalRequest,
    ) -> buck2_error::Result<Outcome> {
        let id = request.id;
        let cancel = Arc::new(EvalCancel::new());
        let fut = run_eval(
            self.sctx,
            thread.jobs(),
            target_cfg.clone(),
            request,
            cancel.dupe(),
            self.emitter.dupe(),
        );
        let outcome = self.in_flight(id, &cancel, None, fut).await;
        if let Ok(Outcome::Eval { reply, .. }) = &outcome {
            self.heap_bytes = reply.heap_bytes;
        }
        outcome
    }

    /// Runs `:build` or `:run`. The outputs of `:build` become `_` once the build is over.
    async fn build(
        &mut self,
        thread: &ReplThread,
        target_cfg: &TargetCfg,
        request: BuildRequest,
    ) -> buck2_error::Result<Outcome> {
        let id = request.id;
        let cancel = Arc::new(EvalCancel::new());
        let fut = run_build(self.sctx, target_cfg.clone(), request, cancel.dupe());
        let mut outcome = self.in_flight(id, &cancel, None, fut).await;
        if let Ok(Outcome::Built {
            result: Ok(Built::Outputs { value, json, .. }),
            ..
        }) = &mut outcome
        {
            if self.json_values {
                *json = serde_json::to_string(value)
                    .ok()
                    .filter(|json| json.len() <= MAX_JSON_BYTES);
            }
            // The transaction is over and the thread is idle.
            match thread.bind("_", std::mem::take(value)).await {
                Some(heap_bytes) => self.heap_bytes = heap_bytes,
                None => return Ok(Outcome::ThreadExited),
            }
        }
        outcome
    }

    /// Runs `:info`, `:ls` or `:__locate`, or checks a target platform.
    async fn inspect(&mut self, request: InspectRequest) -> buck2_error::Result<Outcome> {
        let id = request.id;
        let cancel = Arc::new(EvalCancel::new());
        let fut = run_inspect(self.sctx, request, cancel.dupe());
        self.in_flight(id, &cancel, None, fut).await
    }

    /// `:set`, for the settings of the daemon, which apply from the next request on.
    async fn set(&mut self, id: u64, work: SetWork, title: String, target_cfg: &mut TargetCfg) {
        let setting = match work {
            SetWork::Show(key) => {
                self.send_text(id, &show_settings(target_cfg, key));
                None
            }
            SetWork::Modifiers(modifiers) => {
                target_cfg.cli_modifiers = modifiers;
                Some("modifiers")
            }
            SetWork::TargetPlatforms(platform) if platform.is_empty() => {
                target_cfg.target_platform = platform;
                Some("target_platforms")
            }
            SetWork::TargetPlatforms(platform) => {
                // The platform must be a target: checked (and made absolute) first.
                let request = InspectRequest {
                    id,
                    spec: InspectSpec::Platform { target: platform },
                    title,
                };
                let outcome = self.inspect(request).await;
                if let Ok(Outcome::Inspected {
                    result: Ok(Inspected::Platform(label)),
                    ..
                }) = &outcome
                {
                    target_cfg.target_platform = label.clone();
                    self.emitter.notice(
                        id,
                        repl_notice::Level::Info,
                        setting_changed(target_cfg, "target_platforms"),
                    );
                }
                self.answer_eval(id, outcome);
                return;
            }
        };
        if let Some(setting) = setting {
            self.emitter.notice(
                id,
                repl_notice::Level::Info,
                setting_changed(target_cfg, setting),
            );
        }
        self.done_without_value(id);
    }

    /// Answers request `id`, which has no value.
    fn done_without_value(&self, id: u64) {
        self.emitter.done(
            id,
            ReplDone {
                heap_bytes: self.heap_bytes,
                ..ReplDone::default()
            },
        );
    }

    /// Runs the request `id` to its end while answering the requests that arrive meanwhile.
    ///
    /// While an input runs, other requests get `BUSY` (INV-14). A completion (`completion` is
    /// the session thread, which it does not use) gives way instead: an input cancels it and runs
    /// next, names, attributes and keyword arguments are completed meanwhile (by the thread,
    /// while `fut` is still polled, so that its timeout and its preemption by other commands keep
    /// working), and the functions of the query languages at once; only another completion from
    /// DICE (a target pattern, a module to load, a BXL function) gets `BUSY` (the one in flight
    /// keeps loading, so the next one is fast). The answer to a thread completion is sent
    /// whenever it comes.
    async fn in_flight<T>(
        &mut self,
        id: u64,
        cancel: &EvalCancel,
        completion: Option<&ReplThread>,
        fut: impl Future<Output = T>,
    ) -> T {
        tokio::pin!(fut);
        loop {
            // `fut` is polled until it completes, never dropped (INV-8).
            let event = tokio::select! {
                biased;
                outcome = &mut fut => return outcome,
                _ = &mut self.session_obs, if self.client_open => Event::SessionEnded,
                (answered, answer) = thread_answer(&mut self.completing) => {
                    Event::Completed(answered, answer)
                }
                m = self.req.next(), if self.client_open => Event::Request(m),
            };
            let request = match event {
                Event::SessionEnded | Event::Request(None | Some(Err(_))) => {
                    cancel.trigger();
                    self.client_open = false;
                    continue;
                }
                Event::Completed(answered, answer) => {
                    self.answer_thread_completion(answered, answer);
                    continue;
                }
                Event::Request(Some(Ok(request))) => request,
            };
            match (&request.request, completion) {
                (Some(repl_request::Request::Hangup(_)), _) => {
                    cancel.trigger();
                    self.client_open = false;
                }
                (Some(repl_request::Request::Interrupt(interrupt)), _) => {
                    if interrupt.target_id == id {
                        cancel.trigger();
                    } else if self
                        .queued
                        .as_ref()
                        .is_some_and(|queued| queued.id == interrupt.target_id)
                    {
                        // The input waiting for the completion to wind down.
                        self.queued = None;
                        self.emitter.done(
                            interrupt.target_id,
                            ReplDone {
                                outcome: Some(repl_done::Outcome::Error(failure_proto(
                                    ReplFailure::interrupted(),
                                ))),
                                heap_bytes: self.heap_bytes,
                                ..ReplDone::default()
                            },
                        );
                    }
                }
                (Some(repl_request::Request::Eval(_)), Some(_)) if self.queued.is_none() => {
                    // Enter beats Tab.
                    cancel.trigger();
                    self.queued = Some(request);
                }
                (Some(repl_request::Request::Complete(complete)), Some(thread))
                    if is_thread_completion(complete.kind()) =>
                {
                    // The thread is not used by a completion from DICE.
                    self.start_thread_completion(thread, request.id, complete.clone());
                }
                (Some(repl_request::Request::Complete(complete)), Some(_))
                    if complete.kind() == repl_complete::Kind::Query =>
                {
                    self.emitter.emit(
                        request.id,
                        repl_message::Message::Completions(query_functions(
                            complete.query_dialect(),
                        )),
                    );
                }
                (Some(repl_request::Request::Complete(_)), _) => self.emitter.emit(
                    request.id,
                    repl_message::Message::Completions(completions_status(
                        repl_completions::Status::Busy,
                        "busy: another request of this session is running",
                    )),
                ),
                // One request at a time (INV-14).
                (
                    Some(repl_request::Request::Eval(_) | repl_request::Request::Open(_)) | None,
                    _,
                ) => self.emitter.done(
                    request.id,
                    error_done(
                        repl_error::Kind::Busy,
                        &"busy: another request of this session is running",
                    ),
                ),
            }
        }
    }

    fn answer_init(&mut self, id: u64, outcome: buck2_error::Result<Outcome>) {
        if let Ok(Outcome::Eval {
            reply,
            cwd,
            target_platform,
            ..
        }) = &outcome
            && reply.result.is_ok()
        {
            // `last_equality` stays unset: the first input is never flagged as stale, since
            // no earlier input computed anything (spec §2).
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

    fn answer_reset(&mut self, id: u64, outcome: buck2_error::Result<Outcome>) {
        if let Ok(Outcome::Eval { reply, .. }) = &outcome
            && reply.result.is_ok()
        {
            self.emitter.notice(
                id,
                repl_notice::Level::Info,
                "session reset: every binding and loaded module is gone".to_owned(),
            );
        }
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
                heap_bytes: self.heap_bytes,
                ..ReplDone::default()
            },
            Ok(Outcome::Built {
                result,
                equality,
                t0,
                t1,
                t2,
            }) => {
                let sources_changed = self.last_equality.is_some_and(|last| last != equality);
                self.last_equality = Some(equality);
                let outcome = match result {
                    Ok(Built::Outputs {
                        listing,
                        truncated,
                        json,
                        ..
                    }) => repl_done::Outcome::Value(ReplValue {
                        r#type: "dict".to_owned(),
                        text: listing,
                        truncated,
                        json,
                    }),
                    Ok(Built::Run(run)) => repl_done::Outcome::Run(run),
                    Err(failure) => repl_done::Outcome::Error(failure_proto(failure)),
                };
                ReplDone {
                    outcome: Some(outcome),
                    wait_ms: millis(t1 - t0),
                    eval_ms: millis(t2 - t1),
                    sources_changed,
                    heap_bytes: self.heap_bytes,
                }
            }
            Ok(Outcome::Inspected {
                result,
                equality,
                t0,
                t1,
                t2,
            }) => {
                let sources_changed = self.last_equality.is_some_and(|last| last != equality);
                self.last_equality = Some(equality);
                let outcome = match result {
                    Ok(Inspected::Text(text)) => {
                        self.send_text(id, &text);
                        None
                    }
                    Ok(Inspected::Location(location)) => {
                        Some(repl_done::Outcome::Value(ReplValue {
                            r#type: "location".to_owned(),
                            text: location,
                            truncated: false,
                            json: None,
                        }))
                    }
                    Ok(Inspected::Platform(_)) => None,
                    Err(failure) => Some(repl_done::Outcome::Error(failure_proto(failure))),
                };
                ReplDone {
                    outcome,
                    wait_ms: millis(t1 - t0),
                    eval_ms: millis(t2 - t1),
                    sources_changed,
                    heap_bytes: self.heap_bytes,
                }
            }
            Ok(Outcome::Bxl {
                result,
                equality,
                t0,
                t1,
                t2,
            }) => {
                let sources_changed = self.last_equality.is_some_and(|last| last != equality);
                self.last_equality = Some(equality);
                ReplDone {
                    outcome: result
                        .err()
                        .map(|failure| repl_done::Outcome::Error(failure_proto(failure))),
                    wait_ms: millis(t1 - t0),
                    eval_ms: millis(t2 - t1),
                    sources_changed,
                    heap_bytes: self.heap_bytes,
                }
            }
            Ok(Outcome::Eval {
                reply,
                materialized,
                equality,
                t0,
                t1,
                t2,
                ..
            }) => {
                // Values computed by an earlier input may be stale. The first input has no
                // earlier input to compare with.
                let sources_changed = self.last_equality.is_some_and(|last| last != equality);
                self.last_equality = Some(equality);
                let EvalReply {
                    result,
                    drained,
                    heap_bytes,
                    prelude_loaded: _,
                } = *reply;
                let outcome = match (result, drained, materialized) {
                    (Err(failure), _, _) => Some(repl_done::Outcome::Error(failure_proto(failure))),
                    (Ok(Rendered::Text(text)), Ok(_), Materialized::Done) => {
                        // After the output of the evaluation, as a value would be.
                        self.send_text_as(id, &text.text, text.format);
                        if let Some(incomplete) = text.incomplete {
                            self.emitter
                                .notice(id, repl_notice::Level::Warning, incomplete);
                        }
                        None
                    }
                    (Ok(_), Err(e), _) => Some(repl_done::Outcome::Error(failure_proto(
                        ReplFailure::from_buck2(repl_error::Kind::Buck, &e),
                    ))),
                    (Ok(_), Ok(_), Materialized::Interrupted) => Some(repl_done::Outcome::Error(
                        failure_proto(ReplFailure::interrupted()),
                    )),
                    (Ok(_), Ok(_), Materialized::Failed(errors)) => Some(
                        repl_done::Outcome::Error(failure_proto(materialization_failure(&errors))),
                    ),
                    (Ok(Rendered::Nothing), Ok(_), Materialized::Done) => None,
                    (Ok(Rendered::Value(value)), Ok(_), Materialized::Done) => {
                        Some(repl_done::Outcome::Value(ReplValue {
                            r#type: value.type_name,
                            text: value.text,
                            truncated: value.truncated,
                            json: value.json,
                        }))
                    }
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

    /// Sends plain text to the client's stdout, ending with a newline.
    fn send_text(&self, id: u64, text: &str) {
        self.send_text_as(id, text, repl_output::Format::Plain)
    }

    /// Sends text in `format` to the client's stdout, ending with a newline.
    fn send_text_as(&self, id: u64, text: &str, format: repl_output::Format) {
        let stdout = repl_output::Channel::Stdout;
        self.emitter.output_as(id, stdout, format, text.as_bytes());
        if !text.ends_with('\n') {
            self.emitter.output_as(id, stdout, format, b"\n");
        }
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
                title: title(&eval.input),
                input: eval.input,
                kind: EvalKind::Input,
            }),
            Ok(Some(command)) => match command_work(&command) {
                Ok(CommandWork::Eval { kind, code }) => Work::Eval(EvalRequest {
                    id,
                    number: eval.number,
                    input: code,
                    kind,
                    title: title(&eval.input),
                }),
                Ok(CommandWork::Reset) => Work::Reset { id },
                Ok(CommandWork::Build(spec)) => Work::Build(BuildRequest {
                    id,
                    spec,
                    title: title(&eval.input),
                }),
                Ok(CommandWork::Bxl(spec)) => Work::Bxl(BxlRequest {
                    id,
                    spec,
                    title: title(&eval.input),
                }),
                Ok(CommandWork::Inspect(spec)) => Work::Inspect(InspectRequest {
                    id,
                    spec,
                    title: title(&eval.input),
                }),
                Ok(CommandWork::Set(work)) => Work::Set {
                    id,
                    work,
                    title: title(&eval.input),
                },
                Ok(CommandWork::Text(text, format)) => Work::Text { id, text, format },
                Err(failure) => Work::Reply {
                    id,
                    message: repl_message::Message::Done(ReplDone {
                        outcome: Some(repl_done::Outcome::Error(failure_proto(failure))),
                        ..ReplDone::default()
                    }),
                },
            },
            // The message quotes the token, which may be of any length.
            Err(e) => Work::Reply {
                id,
                message: repl_message::Message::Done(error_done(repl_error::Kind::Usage, &e)),
            },
        },
        repl_request::Request::Complete(req) => match req.kind() {
            kind if is_thread_completion(kind) => Work::CompleteStarlark { id, req },
            repl_complete::Kind::Query => Work::Reply {
                id,
                message: repl_message::Message::Completions(query_functions(req.query_dialect())),
            },
            repl_complete::Kind::TargetPattern => Work::CompleteDice {
                id,
                what: DiceCompletion::Targets { prefix: req.prefix },
            },
            repl_complete::Kind::BxlFunction => Work::CompleteDice {
                id,
                what: DiceCompletion::BxlFunctions {
                    module: req.load_module,
                    prefix: req.prefix,
                },
            },
            repl_complete::Kind::LoadPath => Work::CompleteDice {
                id,
                what: DiceCompletion::LoadPaths { prefix: req.prefix },
            },
            repl_complete::Kind::LoadSymbol => Work::CompleteDice {
                id,
                what: DiceCompletion::LoadSymbols {
                    module: req.load_module,
                    prefix: req.prefix,
                    used: req.used_kwargs,
                },
            },
            _ => Work::Reply {
                id,
                message: repl_message::Message::Completions(completions_status(
                    repl_completions::Status::Error,
                    "this kind of completion is not supported",
                )),
            },
        },
        repl_request::Request::Interrupt(_) => Work::Interrupt,
        repl_request::Request::Hangup(_) => Work::Hangup,
    }
}

/// Whether the session thread answers completions of this kind (from the session's module).
fn is_thread_completion(kind: repl_complete::Kind) -> bool {
    matches!(
        kind,
        repl_complete::Kind::Name | repl_complete::Kind::Attr | repl_complete::Kind::Kwarg
    )
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
    let EvalRequest {
        id,
        number,
        input,
        kind,
        title,
    } = request;
    let t0 = Instant::now();
    let repl_ctx = ReplCtx::eval(sctx, &title);
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
            let target_platform = global_cfg_options.target_platform;

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
                                    kind,
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

            // The artifacts the input ensured, unless it failed (as in `buck2 bxl`). Pending
            // `ctx.output.stream` output is shown as its artifacts are materialized.
            let materialized = match &reply {
                EvalReply {
                    result: Ok(_),
                    drained: Ok(drained),
                    ..
                } if !drained.ensured_artifacts.is_empty()
                    || !drained.pending_streaming_outputs.is_empty() =>
                {
                    if cancel.is_triggered() {
                        Materialized::Interrupted
                    } else {
                        materialize(&txn, &cancel, &emitter, id, drained).await
                    }
                }
                _ => Materialized::Done,
            };

            // `ctx.output.print` output, after the evaluation and the materialization as in
            // `buck2 bxl` (a path it prints exists once it is shown). Streamed output was shown
            // as it was written.
            if let Ok(drained) = &reply.drained {
                emitter.output(id, repl_output::Channel::Stdout, &drained.output);
                emitter.output(id, repl_output::Channel::Stderr, &drained.error);
            }

            Ok(Outcome::Eval {
                reply: Box::new(reply),
                materialized,
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

/// Runs `:build` or `:run` in a transaction of its own. The build races the cancellation of the
/// request: it is DICE work, which is safe to drop.
async fn run_build(
    sctx: &dyn ServerCommandContextTrait,
    target_cfg: TargetCfg,
    request: BuildRequest,
    cancel: Arc<EvalCancel>,
) -> buck2_error::Result<Outcome> {
    let BuildRequest { id: _, spec, title } = request;
    let t0 = Instant::now();
    let repl_ctx = ReplCtx::eval(sctx, &title);
    (&repl_ctx as &dyn ServerCommandContextTrait)
        .with_dice_ctx(|sctx, txn| async move {
            let t1 = Instant::now();
            if cancel.is_triggered() {
                // Cancelled while waiting for other commands.
                return Ok(Outcome::Interrupted { t0, t1 });
            }
            let mut dc = txn.ctx();
            let result = tokio::select! {
                result = build(sctx, &mut dc, &target_cfg, &spec) => result,
                () = cancel.cancelled() => Err(ReplFailure::interrupted()),
            };
            drop(dc);
            Ok(Outcome::Built {
                result,
                equality: txn.equality_token(),
                t0,
                t1,
                t2: Instant::now(),
            })
        })
        .await
}

/// Runs `:info`, `:ls`, `:__locate` or the check of a target platform in a transaction of its
/// own. The work races the cancellation of the request: it is DICE work, which is safe to drop.
async fn run_inspect(
    sctx: &dyn ServerCommandContextTrait,
    request: InspectRequest,
    cancel: Arc<EvalCancel>,
) -> buck2_error::Result<Outcome> {
    let InspectRequest { id: _, spec, title } = request;
    let t0 = Instant::now();
    let repl_ctx = ReplCtx::eval(sctx, &title);
    (&repl_ctx as &dyn ServerCommandContextTrait)
        .with_dice_ctx(|sctx, txn| async move {
            let t1 = Instant::now();
            if cancel.is_triggered() {
                // Cancelled while waiting for other commands.
                return Ok(Outcome::Interrupted { t0, t1 });
            }
            let mut dc = txn.ctx();
            let result = tokio::select! {
                result = inspect(sctx, &mut dc, &spec) => result,
                () = cancel.cancelled() => Err(ReplFailure::interrupted()),
            };
            drop(dc);
            Ok(Outcome::Inspected {
                result,
                equality: txn.equality_token(),
                t0,
                t1,
                t2: Instant::now(),
            })
        })
        .await
}

/// Runs `:bxl` in a transaction of its own. The run races the cancellation of the request: it is
/// DICE work, which is safe to drop.
async fn run_bxl_request(
    sctx: &dyn ServerCommandContextTrait,
    target_cfg: TargetCfg,
    request: BxlRequest,
    cancel: Arc<EvalCancel>,
    emitter: ReplEmitter,
) -> buck2_error::Result<Outcome> {
    let BxlRequest { id, spec, title } = request;
    let t0 = Instant::now();
    let repl_ctx = ReplCtx::eval(sctx, &title);
    (&repl_ctx as &dyn ServerCommandContextTrait)
        .with_dice_ctx(|sctx, txn| async move {
            let t1 = Instant::now();
            if cancel.is_triggered() {
                // Cancelled while waiting for other commands.
                return Ok(Outcome::Interrupted { t0, t1 });
            }
            let result = tokio::select! {
                result = run_bxl(sctx, &txn, &target_cfg, &spec, &emitter, id) => match result {
                    Ok(errors) if errors.is_empty() => Ok(()),
                    Ok(errors) => Err(materialization_failure(&errors)),
                    Err(e) => Err(ReplFailure::from_buck2(repl_error::Kind::Buck, &e)),
                },
                () = cancel.cancelled() => Err(ReplFailure::interrupted()),
            };
            Ok(Outcome::Bxl {
                result,
                equality: txn.equality_token(),
                t0,
                t1,
                t2: Instant::now(),
            })
        })
        .await
}

/// Completes a target pattern, a module to load, a symbol of one, or a BXL function in a
/// transaction of its own, taken with the completion policy (it never waits for commands that
/// use another state, and gives way to them). The work races the cancellation of the request
/// and a timeout: it is DICE computations, which are safe to drop.
async fn run_dice_completion(
    sctx: &dyn ServerCommandContextTrait,
    target_cfg: &TargetCfg,
    what: DiceCompletion,
    cancel: Arc<EvalCancel>,
) -> ReplCompletions {
    let repl_ctx = ReplCtx::completion(sctx);
    let result = (&repl_ctx as &dyn ServerCommandContextTrait)
        .with_dice_ctx(|sctx, txn| async move {
            if cancel.is_triggered() {
                return Ok(completions_status(
                    repl_completions::Status::Cancelled,
                    "cancelled",
                ));
            }
            let mut dc = txn.ctx();
            let work = async {
                let cwd = dc
                    .get_cell_resolver()
                    .await?
                    .get_cell_path(sctx.working_dir());
                match &what {
                    DiceCompletion::Targets { prefix } => {
                        complete_targets(&mut dc, sctx, target_cfg, &cwd, prefix).await
                    }
                    DiceCompletion::BxlFunctions { module, prefix } => {
                        complete_bxl_functions(&mut dc, sctx.working_dir(), module, prefix).await
                    }
                    DiceCompletion::LoadPaths { prefix } => {
                        complete_load_paths(&mut dc, &cwd, prefix).await
                    }
                    DiceCompletion::LoadSymbols {
                        module,
                        prefix,
                        used,
                    } => complete_load_symbols(&mut dc, &cwd, module, prefix, used).await,
                }
            };
            Ok(tokio::select! {
                result = tokio::time::timeout(DICE_COMPLETION_TIMEOUT, work) => match result {
                    Ok(Ok(candidates)) => candidates.into_completions(),
                    Ok(Err(e)) => completion_error(&e),
                    Err(_) => completions_status(
                        repl_completions::Status::Timeout,
                        "timed out",
                    ),
                },
                () = cancel.cancelled() => completions_status(
                    repl_completions::Status::Cancelled,
                    "cancelled",
                ),
            })
        })
        .await;
    match result {
        Ok(answer) => answer,
        Err(e)
            if e.has_tag(buck2_error::ErrorTag::DaemonIsBusy)
                || e.has_tag(buck2_error::ErrorTag::DaemonPreempted) =>
        {
            completions_status(
                repl_completions::Status::Busy,
                "busy: another buck2 command is running with a different state",
            )
        }
        Err(e) => completion_error(&e),
    }
}

/// An answer to a completion that failed.
fn completion_error(e: &buck2_error::Error) -> ReplCompletions {
    let failure = ReplFailure::from_buck2(repl_error::Kind::Buck, e);
    let message = failure
        .message
        .strip_prefix("error: ")
        .unwrap_or(&failure.message);
    completions_status(repl_completions::Status::Error, message)
}

/// Materializes the artifacts an input ensured, racing the cancellation of the request: this is
/// DICE work after the evaluation, which is safe to drop.
async fn materialize(
    txn: &DiceTransaction,
    cancel: &EvalCancel,
    emitter: &ReplEmitter,
    id: u64,
    drained: &OutputStreamOutcome,
) -> Materialized {
    // `wait_on` takes only ensured artifacts, but in a session they may have been ensured by an
    // earlier input: they are materialized again (a no-op if they still are), so that the output
    // waiting on them is shown.
    let mut artifacts = drained.ensured_artifacts.clone();
    for (waits_on, _) in &drained.pending_streaming_outputs {
        artifacts.extend(waits_on.iter().duped());
    }
    let artifacts = artifacts.into_iter().collect();
    let pending = drained
        .pending_streaming_outputs
        .iter()
        .map(|(waits_on, output)| PendingStreamingOutput::new(waits_on.clone(), output.clone()));
    let mut out = ReplOutputWriter::new(emitter.dupe(), id, repl_output::Channel::Stdout);
    let mut dc = txn.ctx();
    let work = txn
        .per_transaction_data()
        .get_dispatcher()
        .dupe()
        .span_async(BxlEnsureArtifactsStart {}, async {
            let result = materialize_ensured_artifacts(
                &mut dc,
                MaterializationAndUploadContext::materialize(),
                artifacts,
                pending,
                &mut out,
            )
            .await;
            (result, BxlEnsureArtifactsEnd {})
        });
    tokio::select! {
        result = work => match result {
            Ok(()) => Materialized::Done,
            Err(errors) => Materialized::Failed(errors),
        },
        () = cancel.cancelled() => Materialized::Interrupted,
    }
}

/// The failure of an input whose ensured artifacts could not all be materialized.
fn materialization_failure(errors: &[buck2_error::Error]) -> ReplFailure {
    struct Errors<'a>(&'a [buck2_error::Error]);
    impl fmt::Display for Errors<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let n = self.0.len();
            write!(
                f,
                "{n} ensured artifact{} could not be materialized",
                if n == 1 { "" } else { "s" }
            )?;
            for e in self.0 {
                write!(f, "\n\n{e:?}")?;
            }
            Ok(())
        }
    }
    ReplFailure::new(repl_error::Kind::Buck, &Errors(errors))
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
