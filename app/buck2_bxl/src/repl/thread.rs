/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! The session thread: it owns the Starlark module of the session and runs the jobs the driver
//! sends it, one at a time.

use buck2_cli_proto::ReplComplete;
use buck2_cli_proto::ReplCompletions;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::global_cfg_options::GlobalCfgOptions;
use buck2_error::buck2_error;
use buck2_events::span::SpanId;
use buck2_interpreter::dice::starlark_provider::StarlarkEvalKind;
use buck2_interpreter::factory::BuckStarlarkModule;
use buck2_interpreter::factory::ProfilingReportedToken;
use buck2_interpreter::factory::StarlarkEvaluatorProvider;
use buck2_util::threads::thread_spawn;
use dice::DiceTransaction;
use dice_futures::cancellation::CancellationContext;
use dice_futures::cancellation::CancellationObserver;
use dupe::Dupe;
use tokio::runtime::Handle;

use crate::bxl::starlark_defs::context::output::OutputStreamOutcome;
use crate::repl::output::ReplEmitter;
use crate::repl::render::RenderMode;
use crate::repl::render::Rendered;
use crate::repl::render::ReplFailure;
use crate::repl::session::Session;

/// Settings of the session, from `ReplOpen`.
#[derive(Clone, Copy)]
pub(crate) struct SessionConfig {
    /// Most bytes the session's heap may hold at its peak.
    pub(crate) heap_limit: usize,
    /// Values get their JSON form too (`buck2 repl --json`).
    pub(crate) json_values: bool,
}

pub(crate) enum Job {
    Eval(EvalJob),
    /// Complete a name, an attribute or a keyword argument. Runs no code and uses no DICE.
    Complete(CompleteJob),
    /// Drops the session's module and starts a new session (`:reset`). Acknowledged once the
    /// old module is gone.
    Reset(tokio::sync::oneshot::Sender<()>),
    Bind(BindJob),
    /// Ends the thread.
    Shutdown,
}

/// What an evaluation job does.
#[derive(Clone, Debug)]
pub(crate) enum EvalKind {
    /// An input: its value is echoed and becomes `_`.
    Input,
    /// Code generated for a meta-command (`:cquery`, `:load` with symbols, ...): like an input,
    /// but errors do not show the generated code.
    Sugar,
    /// `:type`, `:print`, `:json`, `:doc`: the value of the input is rendered in a mode.
    Render(RenderMode),
    /// `:load <module>` without symbols: imports every public symbol of the module (the input
    /// is empty).
    ImportAll { module: String },
    /// `:reload`: loads the modules loaded so far again (the input is empty). With `edited` (an
    /// absolute path, from `:edit`), only if that file is one of them or is loaded by one of
    /// them, and silently.
    Reload { edited: Option<String> },
    /// `:who`: lists the bindings of the session whose names match one of the globs (all if
    /// there are none), with their types (the input is empty).
    Who { globs: Vec<String> },
}

/// Evaluate an input.
pub(crate) struct EvalJob {
    pub(crate) work: EvalWork,
    pub(crate) reply: tokio::sync::oneshot::Sender<EvalReply>,
}

pub(crate) struct EvalWork {
    /// The request.
    pub(crate) id: u64,
    /// The input's number: its code is named `<repl:number>`.
    pub(crate) number: u32,
    pub(crate) input: String,
    pub(crate) kind: EvalKind,
    /// The transaction of the request. The thread drops it, and everything derived from it,
    /// before it replies (INV-4).
    pub(crate) txn: DiceTransaction,
    /// Fires when the request is cancelled. A fresh observer for every job (INV-6).
    pub(crate) liveness: CancellationObserver,
    /// The session's working directory.
    pub(crate) cwd: CellPath,
    pub(crate) global_cfg_options: GlobalCfgOptions,
    /// The span of the request, as the parent of the thread's spans.
    pub(crate) span: Option<SpanId>,
}

/// Complete a name, an attribute or a keyword argument from the session's module.
pub(crate) struct CompleteJob {
    pub(crate) req: ReplComplete,
    pub(crate) reply: tokio::sync::oneshot::Sender<ReplCompletions>,
}

/// Binds a name of the session's module to a value made from JSON (`_` after `:build`).
/// Acknowledged with the bytes allocated on the session's heap.
pub(crate) struct BindJob {
    pub(crate) name: &'static str,
    pub(crate) value: serde_json::Value,
    pub(crate) ack: tokio::sync::oneshot::Sender<u64>,
}

/// How an input went. Only `Send` data.
pub(crate) struct EvalReply {
    /// What to show, or the failure.
    pub(crate) result: Result<Rendered, ReplFailure>,
    /// What `ctx.output` collected, drained after every input (INV-7).
    pub(crate) drained: buck2_error::Result<OutputStreamOutcome>,
    /// Bytes allocated on the session's heap.
    pub(crate) heap_bytes: u64,
    pub(crate) prelude_loaded: bool,
}

/// The driver's handle on the session thread.
pub(crate) struct ReplThread {
    jobs: std::sync::mpsc::Sender<Job>,
    /// Resolves (with an error: its sender is dropped) when the thread exits.
    exited: tokio::sync::oneshot::Receiver<()>,
}

impl ReplThread {
    pub(crate) fn spawn(
        rt: Handle,
        cfg: SessionConfig,
        emitter: ReplEmitter,
    ) -> buck2_error::Result<ReplThread> {
        let (jobs_tx, jobs_rx) = std::sync::mpsc::channel();
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
        thread_spawn("buck2-repl", move || {
            thread_main(rt, jobs_rx, cfg, emitter, exit_tx)
        })
        .map_err(|e| {
            buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "cannot start the repl thread: {e}"
            )
        })?;
        Ok(ReplThread {
            jobs: jobs_tx,
            exited: exit_rx,
        })
    }

    /// A sender for jobs. A send fails only if the thread has exited.
    pub(crate) fn jobs(&self) -> std::sync::mpsc::Sender<Job> {
        self.jobs.clone()
    }

    /// Starts a new session: the thread drops the session's module. Returns whether it did
    /// (`false` if the thread has exited). The driver calls it when no request is in flight (a
    /// completion the thread is answering finishes first).
    pub(crate) async fn reset(&self) -> bool {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        if self.jobs.send(Job::Reset(ack_tx)).is_err() {
            return false;
        }
        ack_rx.await.is_ok()
    }

    /// Binds `name` to `value` (made into a Starlark value) in the session's module. Returns the
    /// bytes allocated on the session's heap, or `None` if the thread has exited. The driver
    /// calls it when no request is in flight (a completion the thread is answering finishes first).
    pub(crate) async fn bind(&self, name: &'static str, value: serde_json::Value) -> Option<u64> {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let job = Job::Bind(BindJob {
            name,
            value,
            ack: ack_tx,
        });
        if self.jobs.send(job).is_err() {
            return None;
        }
        ack_rx.await.ok()
    }

    /// Starts the completion of a name, an attribute or a keyword argument, and returns where
    /// its answer comes; the receiver fails if the thread exits first. `None` if the thread has
    /// exited. Jobs run in the order they are sent, so a job sent after this one starts once it
    /// is answered.
    pub(crate) fn start_complete(
        &self,
        req: ReplComplete,
    ) -> Option<tokio::sync::oneshot::Receiver<ReplCompletions>> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let job = Job::Complete(CompleteJob {
            req,
            reply: reply_tx,
        });
        self.jobs.send(job).ok()?;
        Some(reply_rx)
    }

    /// Asks the thread to exit and waits until it has (INV-16). The driver calls it when no job
    /// is in flight.
    pub(crate) async fn shutdown(self) {
        let ReplThread { jobs, exited } = self;
        let _ignored = jobs.send(Job::Shutdown);
        drop(jobs);
        // Only `Err` (the sender is dropped when the thread exits).
        let _ignored = exited.await;
    }
}

fn thread_main(
    rt: Handle,
    jobs: std::sync::mpsc::Receiver<Job>,
    cfg: SessionConfig,
    emitter: ReplEmitter,
    exit_tx: tokio::sync::oneshot::Sender<()>,
) {
    // Dropped when the thread exits, however it exits, which wakes `shutdown`.
    let _exit = exit_tx;
    // For the whole life of the thread (INV-3): `BxlDiceComputations::via` blocks on the
    // current runtime. Nothing here runs inside a `block_on` while Starlark is on the stack.
    let _rt = rt.enter();
    // A session per module: `:reset` ends one and starts the next.
    loop {
        let mut reset = None;
        let _ignored: buck2_error::Result<()> = BuckStarlarkModule::with_profiling(|env| {
            let mut session = Session::new(rt.clone(), cfg, emitter.dupe());
            loop {
                match jobs.recv() {
                    Err(_) | Ok(Job::Shutdown) => break,
                    Ok(Job::Reset(ack)) => {
                        reset = Some(ack);
                        break;
                    }
                    Ok(Job::Eval(EvalJob { work, reply })) => {
                        // `eval_job` consumes the work, so its DICE handles are gone before the
                        // reply is sent (INV-4).
                        let result = session.eval_job(&env, work);
                        let _ignored = reply.send(result);
                    }
                    Ok(Job::Complete(CompleteJob { req, reply })) => {
                        let _ignored = reply.send(session.complete(&env, &req));
                    }
                    Ok(Job::Bind(BindJob { name, value, ack })) => {
                        // No evaluator runs, so nothing can collect the session's heap (INV-1).
                        // The value is only as deep as JSON made by the driver.
                        env.set(name, env.heap().alloc(value));
                        let _ignored = ack.send(heap_bytes(&env));
                    }
                }
            }
            let token = match session.take_token() {
                Some(token) => token,
                None => mint_token(&env)?,
            };
            Ok((token, ()))
        });
        // The module of the session is gone.
        match reset {
            Some(ack) => {
                let _ignored = ack.send(());
            }
            None => break,
        }
    }
}

/// Bytes allocated on the session's heap.
pub(crate) fn heap_bytes(env: &BuckStarlarkModule<'_>) -> u64 {
    u64::try_from(env.heap().allocated_bytes()).unwrap_or(u64::MAX)
}

/// A profiling token for a module that no evaluation has finished with. It runs no code.
fn mint_token(env: &BuckStarlarkModule) -> buck2_error::Result<ProfilingReportedToken> {
    let (finished, ()) =
        StarlarkEvaluatorProvider::passthrough(StarlarkEvalKind::Unknown("repl".into()))
            .with_evaluator(
                env,
                CancellationContext::never_cancelled().into(),
                |_, _| Ok(()),
            )?;
    Ok(finished.finish()?.0)
}
