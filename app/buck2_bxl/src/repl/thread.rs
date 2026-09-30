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
use tokio::runtime::Handle;

use crate::bxl::starlark_defs::context::output::OutputStreamOutcome;
use crate::repl::output::ReplEmitter;
use crate::repl::render::RenderedValue;
use crate::repl::render::ReplFailure;
use crate::repl::session::Session;

/// Settings of the session, from `ReplOpen`.
pub(crate) struct SessionConfig {
    /// Most bytes the session's heap may hold at its peak.
    pub(crate) heap_limit: usize,
}

pub(crate) enum Job {
    Eval(EvalJob),
    /// Ends the thread.
    Shutdown,
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

/// How an input went. Only `Send` data.
pub(crate) struct EvalReply {
    /// The value to echo (`None` for `None`), or the failure.
    pub(crate) result: Result<Option<RenderedValue>, ReplFailure>,
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
    let _ignored: buck2_error::Result<()> = BuckStarlarkModule::with_profiling(|env| {
        let mut session = Session::new(rt.clone(), cfg, emitter);
        loop {
            match jobs.recv() {
                Err(_) | Ok(Job::Shutdown) => break,
                Ok(Job::Eval(EvalJob { work, reply })) => {
                    // `eval_job` consumes the work, so its DICE handles are gone before the
                    // reply is sent (INV-4).
                    let result = session.eval_job(&env, work);
                    let _ignored = reply.send(result);
                }
            }
        }
        let token = match session.take_token() {
            Some(token) => token,
            None => mint_token(&env)?,
        };
        Ok((token, ()))
    });
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
