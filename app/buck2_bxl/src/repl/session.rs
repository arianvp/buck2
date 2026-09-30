/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! The session on its thread: evaluation of inputs in the session's module.

use std::collections::HashMap;
use std::future::Future;
use std::pin::pin;

use buck2_cli_proto::repl_error;
use buck2_cli_proto::repl_notice;
use buck2_common::events::HasEvents;
use buck2_core::bxl::BxlFilePath;
use buck2_error::buck2_error;
use buck2_events::dispatch::EventDispatcher;
use buck2_events::dispatch::maybe_proxy_current_span;
use buck2_events::dispatch::with_dispatcher;
use buck2_events::dispatch::with_dispatcher_async;
use buck2_fs::paths::forward_rel_path::ForwardRelativePath;
use buck2_interpreter::factory::BuckStarlarkModule;
use buck2_interpreter::factory::FinishedStarlarkEvaluation;
use buck2_interpreter::factory::ProfilingReportedToken;
use buck2_interpreter::factory::StarlarkEvaluatorProvider;
use buck2_interpreter::soft_error::Buck2StarlarkSoftErrorHandler;
use buck2_repl_syntax::text::PrecheckError;
use buck2_repl_syntax::text::dedent;
use buck2_repl_syntax::text::precheck;
use dice_futures::cancellation::CancellationObserver;
use dupe::Dupe;
use futures::future::Either;
use futures::future::select;
use starlark::PrintHandler;
use starlark::environment::FrozenModule;
use starlark::eval::Evaluator;
use starlark::eval::FileLoader;
use starlark::values::ValueOfUnchecked;
use starlark::values::structs::AllocStruct;
use tokio::runtime::Handle;

use crate::bxl::starlark_defs::context::BxlContext;
use crate::bxl::starlark_defs::context::output::OutputStreamState;
use crate::bxl::starlark_defs::context::starlark_async::BxlDiceComputations;
use crate::bxl::starlark_defs::eval_extra::BxlEvalExtra;
use crate::repl::output::ReplEmitter;
use crate::repl::output::ReplPrintHandler;
use crate::repl::prep::PrepRequest;
use crate::repl::prep::Prepared;
use crate::repl::prep::prepare;
use crate::repl::render::RenderedValue;
use crate::repl::render::ReplFailure;
use crate::repl::render::render_echo;
use crate::repl::thread::EvalReply;
use crate::repl::thread::EvalWork;
use crate::repl::thread::SessionConfig;

/// The synthetic `.bxl` file of the session, in its working directory. It is never read; loads
/// resolve relative to it.
const REPL_FILE_NAME: &str = "__repl__.bxl";

/// The state of the session between jobs. It holds no Starlark values (INV-2): values live only
/// in the module's slots.
pub(crate) struct Session {
    rt: Handle,
    cfg: SessionConfig,
    emitter: ReplEmitter,
    /// The output state of every `ctx` of the session (INV-7), so that a `ctx` (or anything
    /// holding one) from an earlier input still works. Drained after every job.
    stream: OutputStreamState,
    /// The prelude has yet to be imported (or to fail to).
    need_prelude: bool,
    prelude_loaded: bool,
    /// The token of the last finished evaluation, to leave the module's profiling scope with.
    last_token: Option<ProfilingReportedToken>,
}

impl Session {
    pub(crate) fn new(rt: Handle, cfg: SessionConfig, emitter: ReplEmitter) -> Self {
        Session {
            rt,
            cfg,
            emitter,
            stream: OutputStreamState::new(),
            need_prelude: true,
            prelude_loaded: false,
            last_token: None,
        }
    }

    pub(crate) fn take_token(&mut self) -> Option<ProfilingReportedToken> {
        self.last_token.take()
    }

    /// Evaluates an input. Consumes the work, so every DICE handle is dropped when this returns
    /// (INV-4).
    pub(crate) fn eval_job(&mut self, env: &BuckStarlarkModule<'_>, work: EvalWork) -> EvalReply {
        let dispatcher = work.txn.per_transaction_data().get_dispatcher().dupe();
        // Per job, on this thread: events of the evaluation go to the request's dispatcher.
        maybe_proxy_current_span(work.span, || {
            with_dispatcher(dispatcher.dupe(), || {
                let print = ReplPrintHandler::new(self.emitter.dupe(), work.id);
                let result = self.eval_inner(env, work, &print, dispatcher);
                // On every path, success, error or interrupt (INV-7).
                print.flush();
                let drained = self.stream.drain();
                EvalReply {
                    result,
                    drained,
                    heap_bytes: u64::try_from(env.heap().allocated_bytes()).unwrap_or(u64::MAX),
                    prelude_loaded: self.prelude_loaded,
                }
            })
        })
    }

    fn eval_inner(
        &mut self,
        env: &BuckStarlarkModule<'_>,
        work: EvalWork,
        print: &ReplPrintHandler,
        dispatcher: EventDispatcher,
    ) -> Result<Option<RenderedValue>, ReplFailure> {
        let EvalWork {
            id,
            number,
            input,
            txn,
            liveness,
            cwd,
            global_cfg_options,
            span: _,
        } = work;

        let code = dedent(&input);
        precheck(&code).map_err(|e| {
            let kind = match e {
                PrecheckError::TooLarge { .. } => repl_error::Kind::Usage,
                _ => repl_error::Kind::Syntax,
            };
            ReplFailure::new(kind, &e)
        })?;
        let code = code.into_owned();

        let repl_path = ForwardRelativePath::new(REPL_FILE_NAME)
            .and_then(|name| BxlFilePath::new(cwd.join(name)))
            .map_err(|e| ReplFailure::from_buck2(repl_error::Kind::Internal, &e))?;

        let mut dc = txn.ctx();
        let prepared = self.block_on(
            &liveness,
            dispatcher,
            prepare(
                &mut dc,
                PrepRequest {
                    number,
                    code,
                    repl_path: &repl_path,
                    cwd,
                    global_cfg_options,
                    need_prelude: self.need_prelude,
                },
            ),
        )?;
        let Prepared {
            globals,
            ast,
            loads,
            prelude,
            core,
            provider,
            digest_config,
        } = prepared;

        if let Some(prelude) = prelude {
            self.need_prelude = false;
            match prelude {
                Ok(modules) => {
                    for module in &modules {
                        env.import_public_symbols(module);
                    }
                    self.prelude_loaded = !modules.is_empty();
                }
                Err(e) => {
                    let failure = ReplFailure::from_buck2(repl_error::Kind::Buck, &e);
                    let reason = failure
                        .message
                        .strip_prefix("error: ")
                        .unwrap_or(&failure.message);
                    self.emitter.notice(
                        id,
                        repl_notice::Level::Warning,
                        format!(
                            "the prelude could not be loaded, so its symbols are not defined: {reason}"
                        ),
                    );
                }
            }
        }

        let loader = ReplLoader(loads);
        let stream = self.stream.dupe();
        let mut extra = BxlEvalExtra::new(
            BxlDiceComputations::new(&mut dc, liveness.dupe()),
            core.dupe(),
            self.stream.dupe(),
        );
        let heap_limit = self.cfg.heap_limit;
        let (finished, result) = with_repl_evaluator(
            env,
            provider,
            &liveness,
            EvaluatorSetup {
                loader: &loader,
                print,
                extra: &mut extra,
                heap_limit,
            },
            |eval| {
                let heap = eval.heap();
                let cli_args = ValueOfUnchecked::new(heap.alloc(AllocStruct::EMPTY));
                let ctx = BxlContext::new(heap, core, stream, cli_args, digest_config)?;
                env.set("ctx", heap.alloc(ctx));
                Ok(match eval.eval_module(ast, &globals) {
                    Ok(v) => {
                        let rendered = render_echo(v);
                        if !v.is_none() {
                            env.set("_", v);
                        }
                        Ok(rendered)
                    }
                    Err(e) => Err(e),
                })
            },
        )
        .map_err(|e| ReplFailure::from_buck2(repl_error::Kind::Internal, &e))?;
        self.last_token = Some(
            finished
                .finish()
                .map_err(|e| ReplFailure::from_buck2(repl_error::Kind::Internal, &e))?
                .0,
        );

        result.map_err(|e| {
            if liveness.is_cancelled() {
                ReplFailure::interrupted()
            } else {
                let mut failure = ReplFailure::from_starlark(e);
                if env.heap().peak_allocated_bytes() >= heap_limit {
                    failure.add_note(&format_args!(
                        "the session's heap is limited to {} MiB (`--max-heap-mb`) and is full; \
                         start a new session (`:reset`, or a new `buck2 repl`)",
                        heap_limit >> 20
                    ));
                }
                failure
            }
        })
    }

    /// Runs the asynchronous part of an evaluation to completion on this thread, or until the
    /// request is cancelled. No Starlark is on the stack here (INV-3).
    fn block_on<T>(
        &self,
        liveness: &CancellationObserver,
        dispatcher: EventDispatcher,
        fut: impl Future<Output = Result<T, ReplFailure>>,
    ) -> Result<T, ReplFailure> {
        let liveness = liveness.dupe();
        self.rt
            .block_on(with_dispatcher_async(dispatcher, async move {
                let fut = pin!(fut);
                let liveness = pin!(liveness);
                match select(fut, liveness).await {
                    Either::Left((result, _)) => result,
                    Either::Right(((), _)) => Err(ReplFailure::interrupted()),
                }
            }))
    }
}

/// Creates the evaluator of an input. The only place that creates an evaluator that runs code,
/// so every one has garbage collection off (INV-1) and `BxlEvalExtra` set (INV-5).
fn with_repl_evaluator<'v, 'a, 'e: 'a, R>(
    env: &'a BuckStarlarkModule<'v>,
    provider: StarlarkEvaluatorProvider,
    liveness: &CancellationObserver,
    setup: EvaluatorSetup<'a, 'e>,
    f: impl FnOnce(&mut Evaluator<'v, 'a, 'e>) -> buck2_error::Result<R>,
) -> buck2_error::Result<(FinishedStarlarkEvaluation, R)> {
    let EvaluatorSetup {
        loader,
        print,
        extra,
        heap_limit,
    } = setup;
    provider.with_evaluator(env, liveness.into(), |eval, _| {
        // First, always: values of BXL types may hold untraced references, and a new
        // evaluator would collect as soon as the session's heap is over 100 KB.
        eval.disable_gc();
        eval.set_max_heap_size(heap_limit).map_err(|e| {
            buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "cannot limit the heap of the repl: {e}"
            )
        })?;
        eval.set_print_handler(print);
        eval.set_soft_error_handler(&Buck2StarlarkSoftErrorHandler);
        eval.set_loader(loader);
        eval.extra_mut = Some(extra);
        f(eval)
    })
}

/// What the evaluator of an input is given.
struct EvaluatorSetup<'a, 'e> {
    loader: &'a dyn FileLoader,
    print: &'a dyn PrintHandler,
    extra: &'a mut BxlEvalExtra<'e>,
    /// Most bytes the session's heap may hold at its peak.
    heap_limit: usize,
}

/// Serves the modules an input loads, resolved and loaded beforehand.
struct ReplLoader(HashMap<String, FrozenModule>);

impl FileLoader for ReplLoader {
    fn load(&self, path: &str) -> starlark::Result<FrozenModule> {
        match self.0.get(path) {
            Some(module) => Ok(module.dupe()),
            None => Err(buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "module `{path}` was not loaded before the evaluation"
            )
            .into()),
        }
    }
}
