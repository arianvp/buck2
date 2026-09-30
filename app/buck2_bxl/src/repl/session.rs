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
use std::fmt;
use std::future::Future;
use std::pin::pin;

use buck2_cli_proto::ReplComplete;
use buck2_cli_proto::ReplCompletions;
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
use buck2_repl_syntax::text::CappedString;
use buck2_repl_syntax::text::PrecheckError;
use buck2_repl_syntax::text::dedent;
use buck2_repl_syntax::text::precheck;
use buck2_repl_syntax::text::starlark_string_literal;
use dice_futures::cancellation::CancellationObserver;
use dupe::Dupe;
use futures::future::Either;
use futures::future::select;
use starlark::PrintHandler;
use starlark::environment::FrozenModule;
use starlark::environment::Globals;
use starlark::eval::Evaluator;
use starlark::eval::FileLoader;
use starlark::values::ValueOfUnchecked;
use starlark::values::structs::AllocStruct;
use tokio::runtime::Handle;

use crate::bxl::starlark_defs::context::BxlContext;
use crate::bxl::starlark_defs::context::output::OutputStreamState;
use crate::bxl::starlark_defs::context::starlark_async::BxlDiceComputations;
use crate::bxl::starlark_defs::eval_extra::BxlEvalExtra;
use crate::repl::complete::names::complete_starlark;
use crate::repl::complete::private::PrivateBindings;
use crate::repl::complete::types::TypeIndex;
use crate::repl::docstrings::Docstrings;
use crate::repl::output::ReplEmitter;
use crate::repl::output::ReplPrintHandler;
use crate::repl::prep::PrepRequest;
use crate::repl::prep::Prepared;
use crate::repl::prep::input_file_name;
use crate::repl::prep::prepare;
use crate::repl::render::RenderBudget;
use crate::repl::render::RenderContext;
use crate::repl::render::RenderMode;
use crate::repl::render::Rendered;
use crate::repl::render::ReplFailure;
use crate::repl::render::render;
use crate::repl::thread::EvalKind;
use crate::repl::thread::EvalReply;
use crate::repl::thread::EvalWork;
use crate::repl::thread::SessionConfig;
use crate::repl::thread::heap_bytes;

/// The synthetic `.bxl` file of the session, in its working directory. It is never read; loads
/// resolve relative to it.
const REPL_FILE_NAME: &str = "__repl__.bxl";

/// Most names a notice about loaded modules lists.
const MAX_NOTICE_NAMES: usize = 64;

/// Longest notice about loaded modules.
const MAX_NOTICE_BYTES: usize = 4 << 10;

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
    /// What `:reload` loads again.
    loaded: LoadedModules,
    /// The globals of the last evaluation, for completion.
    globals: Option<Globals>,
    /// The documentation of the types of the globals, built when `:doc` or completion first
    /// needs it.
    types: Option<TypeIndex>,
    /// The docstrings of the functions defined in the session, for `:doc`.
    docstrings: Docstrings,
    /// Where the private bindings (loaded symbols) come from, for completion.
    private: PrivateBindings,
}

/// The modules the session has loaded, to load again on `:reload`.
#[derive(Default)]
struct LoadedModules {
    /// Modules imported whole (`:load` without symbols), in order.
    import_all: Vec<String>,
    /// `load` statements: the modules (as they were loaded), each with its symbols (local name,
    /// name in the module), in the order they were first loaded. A local name is bound by the
    /// latest load only.
    loads: Vec<(String, Vec<(String, String)>)>,
}

impl LoadedModules {
    fn is_empty(&self) -> bool {
        self.import_all.is_empty() && self.loads.is_empty()
    }

    fn add_import_all(&mut self, module: &str) {
        if !self.import_all.iter().any(|m| m == module) {
            self.import_all.push(module.to_owned());
        }
    }

    fn add_load(&mut self, module: &str, symbols: &[(String, String)]) {
        for (local, _) in symbols {
            for (_, known) in &mut self.loads {
                known.retain(|(l, _)| l != local);
            }
        }
        match self.loads.iter_mut().find(|(m, _)| m == module) {
            Some((_, known)) => known.extend(symbols.iter().cloned()),
            None => self.loads.push((module.to_owned(), symbols.to_vec())),
        }
        self.loads.retain(|(_, symbols)| !symbols.is_empty());
    }

    /// The `load` statements that bind the loaded symbols again.
    fn load_statements(&self) -> String {
        let mut code = String::new();
        for (module, symbols) in &self.loads {
            code.push_str("load(");
            code.push_str(&starlark_string_literal(module));
            for (local, symbol) in symbols {
                code.push_str(", ");
                if local != symbol {
                    code.push_str(local);
                    code.push_str(" = ");
                }
                code.push_str(&starlark_string_literal(symbol));
            }
            code.push_str(")\n");
        }
        code
    }

    /// Every module, for notices.
    fn modules(&self) -> impl Iterator<Item = &str> {
        self.import_all
            .iter()
            .map(String::as_str)
            .chain(self.loads.iter().map(|(m, _)| m.as_str()))
    }
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
            loaded: LoadedModules::default(),
            globals: None,
            types: None,
            docstrings: Docstrings::default(),
            private: PrivateBindings::default(),
        }
    }

    pub(crate) fn take_token(&mut self) -> Option<ProfilingReportedToken> {
        self.last_token.take()
    }

    /// Completes a name or an attribute. Runs no code (INV-1) and uses no DICE.
    pub(crate) fn complete(
        &mut self,
        env: &BuckStarlarkModule<'_>,
        req: &ReplComplete,
    ) -> ReplCompletions {
        let globals = self.globals.as_ref();
        if self.types.is_none()
            && let Some(globals) = globals
        {
            self.types = Some(TypeIndex::build(globals));
        }
        complete_starlark(env, &self.private, globals, self.types.as_ref(), req)
    }

    /// Evaluates an input. Consumes the work, so every DICE handle is dropped when this returns
    /// (INV-4).
    pub(crate) fn eval_job(&mut self, env: &BuckStarlarkModule<'_>, work: EvalWork) -> EvalReply {
        let dispatcher = work.txn.per_transaction_data().get_dispatcher().dupe();
        // Per job, on this thread: events of the evaluation go to the request's dispatcher.
        maybe_proxy_current_span(work.span, || {
            with_dispatcher(dispatcher.dupe(), || {
                let print = ReplPrintHandler::new(&self.rt, self.emitter.dupe(), work.id);
                let result = self.eval_inner(env, work, &print, dispatcher);
                // On every path, success, error or interrupt (INV-7).
                print.flush();
                let drained = self.stream.drain();
                EvalReply {
                    result,
                    drained,
                    heap_bytes: heap_bytes(env),
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
    ) -> Result<Rendered, ReplFailure> {
        let EvalWork {
            id,
            number,
            input,
            kind,
            txn,
            liveness,
            cwd,
            global_cfg_options,
            span: _,
        } = work;

        let (code, import_all) = match &kind {
            EvalKind::ImportAll { module } => (String::new(), vec![module.clone()]),
            EvalKind::Reload => {
                if self.loaded.is_empty() {
                    self.emitter.notice(
                        id,
                        repl_notice::Level::Info,
                        "nothing to reload: no module has been loaded".to_owned(),
                    );
                    return Ok(Rendered::Nothing);
                }
                (
                    self.loaded.load_statements(),
                    self.loaded.import_all.clone(),
                )
            }
            EvalKind::Input | EvalKind::Sugar | EvalKind::Render(_) => (input, Vec::new()),
        };
        // Errors in code the user did not write are shown without it.
        let generated = matches!(kind, EvalKind::Sugar | EvalKind::Reload);
        let mode = match kind {
            EvalKind::Render(mode) => mode,
            _ => RenderMode::Echo,
        };

        let code = dedent(&code);
        precheck(&code).map_err(|e| {
            let kind = match e {
                PrecheckError::TooLarge { .. } => repl_error::Kind::Usage,
                _ => repl_error::Kind::Syntax,
            };
            ReplFailure::new(kind, &e)
        })?;
        let code = code.into_owned();
        // For the heading of `:doc`.
        let typed = if mode == RenderMode::Doc {
            code.clone()
        } else {
            String::new()
        };

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
                    import_all,
                },
            ),
        )?;
        let Prepared {
            globals,
            ast,
            loads,
            import_all,
            prelude,
            core,
            provider,
            digest_config,
        } = prepared;

        if let Some(prelude) = prelude {
            self.need_prelude = false;
            match prelude {
                Ok(modules) => {
                    for (i, module) in modules.iter().enumerate() {
                        env.import_public_symbols(module);
                        self.private.import_all(&format!("<prelude {i}>"), module);
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

        // Modules imported whole: their public symbols become (private) bindings of the session,
        // as with `load`.
        for (module_id, module) in &import_all {
            env.import_public_symbols(module);
            self.private.import_all(module_id, module);
        }
        // The `load` statements of the code, recorded for `:reload` once they have run.
        let load_statements: Vec<(String, Vec<(String, String)>)> = ast
            .loads()
            .iter()
            .map(|load| {
                (
                    load.module_id.to_owned(),
                    load.symbols
                        .iter()
                        .map(|(local, symbol)| ((*local).to_owned(), (*symbol).to_owned()))
                        .collect(),
                )
            })
            .collect();

        self.globals = Some(globals.dupe());
        if mode == RenderMode::Doc && self.types.is_none() {
            self.types = Some(TypeIndex::build(&globals));
        }
        let types = self.types.as_ref();
        // Before the evaluation, which may define some of the functions and then fail.
        self.docstrings.record(&ast, &input_file_name(number));
        let docstrings = &self.docstrings;

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
                let ctx = BxlContext::new(heap, core.dupe(), stream, cli_args, digest_config)?;
                env.set("ctx", heap.alloc(ctx));
                Ok(match eval.eval_module(ast, &globals) {
                    Ok(v) => {
                        let cancelled = || liveness.is_cancelled();
                        let cx = RenderContext {
                            core: &core,
                            docstrings,
                            heap,
                            budget: RenderBudget::new(&cancelled),
                            types,
                            code: &typed,
                        };
                        let rendered = render(v, mode, &cx);
                        if rendered.is_ok() && mode.binds_last_value() && !v.is_none() {
                            env.set("_", v);
                        }
                        Ok(rendered)
                    }
                    Err(e) => Err(e),
                })
            },
        )
        .map_err(|e| ReplFailure::from_buck2(repl_error::Kind::Internal, &e))?;
        // Whether the evaluation succeeded or not, the loads it made are bindings now. `load`
        // statements run in order, and the first symbol that cannot be loaded stops the
        // evaluation: the symbols after it are not bound. (A `load` after another statement that
        // failed is recorded all the same: which statement failed is not known here.)
        'loads: for (module_id, symbols) in &load_statements {
            let Some(module) = loader.0.get(module_id) else {
                break;
            };
            for (local, symbol) in symbols {
                if PrivateBindings::exported(module, symbol).is_none() {
                    break 'loads;
                }
                self.private.load(local, module, symbol);
            }
        }
        self.last_token = Some(
            finished
                .finish()
                .map_err(|e| ReplFailure::from_buck2(repl_error::Kind::Internal, &e))?
                .0,
        );

        let rendered = result.map_err(|e| {
            if liveness.is_cancelled() {
                ReplFailure::interrupted()
            } else {
                let mut failure = if generated {
                    ReplFailure::from_starlark(starlark::Error::new_kind(e.into_kind()))
                } else {
                    ReplFailure::from_starlark(e)
                };
                if env.heap().peak_allocated_bytes() >= heap_limit {
                    failure.add_note(&format_args!(
                        "the session's heap is limited to {} MiB (`--max-heap-mb`) and is full; \
                         start a new session (`:reset`, or a new `buck2 repl`)",
                        heap_limit >> 20
                    ));
                }
                failure
            }
        })??;

        // The loads ran: record them for `:reload`.
        for (module, symbols) in &load_statements {
            self.loaded.add_load(module, symbols);
        }
        match &kind {
            EvalKind::ImportAll { .. } => {
                for (module_id, module) in &import_all {
                    self.loaded.add_import_all(module_id);
                    self.emitter.notice(
                        id,
                        repl_notice::Level::Info,
                        loaded_notice(module_id, module),
                    );
                }
            }
            EvalKind::Reload => {
                let mut notice = CappedString::new(MAX_NOTICE_BYTES);
                let _ignored = fmt::write(&mut notice, format_args!("reloaded"));
                for (i, module) in self.loaded.modules().enumerate() {
                    let sep = if i == 0 { " " } else { ", " };
                    let _ignored = fmt::write(&mut notice, format_args!("{sep}{module}"));
                }
                self.emitter
                    .notice(id, repl_notice::Level::Info, notice.into_string());
            }
            EvalKind::Input | EvalKind::Sugar | EvalKind::Render(_) => {}
        }
        Ok(rendered)
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

/// `loaded //pkg:x.bzl: a, b, c`: what `:load` without symbols imported.
fn loaded_notice(module_id: &str, module: &FrozenModule) -> String {
    let mut notice = CappedString::new(MAX_NOTICE_BYTES);
    let _ignored = fmt::write(&mut notice, format_args!("loaded {module_id}:"));
    let mut names: Vec<&str> = module.names().filter(|n| !n.starts_with('_')).collect();
    names.sort_unstable();
    if names.is_empty() {
        let _ignored = fmt::write(&mut notice, format_args!(" no public symbols"));
    }
    for (i, name) in names.iter().take(MAX_NOTICE_NAMES).enumerate() {
        let sep = if i == 0 { " " } else { ", " };
        let _ignored = fmt::write(&mut notice, format_args!("{sep}{name}"));
    }
    if names.len() > MAX_NOTICE_NAMES {
        let _ignored = fmt::write(
            &mut notice,
            format_args!(" and {} more", names.len() - MAX_NOTICE_NAMES),
        );
    }
    notice.into_string()
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
