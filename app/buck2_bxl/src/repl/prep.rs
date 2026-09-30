/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! The asynchronous part of an evaluation, which runs before any Starlark is on the stack of the
//! session thread (INV-3): the globals, parsing, the modules the input loads, the prelude, and
//! the data behind `ctx`.

use std::collections::HashMap;
use std::sync::Arc;

use buck2_build_api::bxl::types::BxlFunctionLabel;
use buck2_cli_proto::repl_error;
use buck2_core::bxl::BxlFilePath;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::global_cfg_options::GlobalCfgOptions;
use buck2_execute::digest_config::DigestConfig;
use buck2_execute::digest_config::HasDigestConfig;
use buck2_interpreter::dice::starlark_provider::StarlarkEvalKind;
use buck2_interpreter::factory::StarlarkEvaluatorProvider;
use buck2_interpreter::file_type::StarlarkFileType;
use buck2_interpreter::load_module::InterpreterCalculation;
use buck2_interpreter::paths::module::OwnedStarlarkModulePath;
use buck2_interpreter::paths::path::OwnedStarlarkPath;
use buck2_interpreter::paths::path::StarlarkPath;
use buck2_interpreter_for_build::interpreter::dice_calculation_delegate::HasCalculationDelegate;
use buck2_interpreter_for_build::interpreter::global_interpreter_state::HasGlobalInterpreterState;
use buck2_interpreter_for_build::interpreter::interpreter_for_dir::ParseData;
use dice::DiceComputations;
use dupe::Dupe;
use starlark::environment::FrozenModule;
use starlark::environment::Globals;
use starlark::syntax::AstModule;
use starlark_map::ordered_map::OrderedMap;

use crate::bxl::key::BxlKey;
use crate::bxl::starlark_defs::context::BxlContextCoreData;
use crate::repl::render::ReplFailure;

/// The name of the synthetic BXL function of the session.
const REPL_FUNCTION_NAME: &str = "__repl__";

pub(crate) struct PrepRequest<'a> {
    /// The input's number: its code is named `<repl:number>`.
    pub(crate) number: u32,
    /// The input, dedented and prechecked.
    pub(crate) code: String,
    /// The session's synthetic `.bxl` file (never read): loads resolve relative to it.
    pub(crate) repl_path: &'a BxlFilePath,
    pub(crate) cwd: CellPath,
    pub(crate) global_cfg_options: GlobalCfgOptions,
    /// Load the prelude.
    pub(crate) need_prelude: bool,
    /// Modules to import whole (`:load` without symbols), as they would be loaded.
    pub(crate) import_all: Vec<String>,
}

pub(crate) struct Prepared {
    pub(crate) globals: Globals,
    pub(crate) ast: AstModule,
    /// The modules the input loads, by the string it loads them with.
    pub(crate) loads: HashMap<String, FrozenModule>,
    /// The modules to import whole, in the order asked for, with the string they were asked
    /// for with.
    pub(crate) import_all: Vec<(String, FrozenModule)>,
    /// The prelude's modules if asked for (none if the project has no prelude), or why they
    /// could not be loaded.
    pub(crate) prelude: Option<buck2_error::Result<Vec<FrozenModule>>>,
    pub(crate) core: Arc<BxlContextCoreData>,
    pub(crate) provider: StarlarkEvaluatorProvider,
    pub(crate) digest_config: DigestConfig,
}

fn buck(e: buck2_error::Error) -> ReplFailure {
    ReplFailure::from_buck2(repl_error::Kind::Buck, &e)
}

pub(crate) async fn prepare(
    dc: &mut DiceComputations<'_>,
    req: PrepRequest<'_>,
) -> Result<Prepared, ReplFailure> {
    let PrepRequest {
        number,
        code,
        repl_path,
        cwd,
        global_cfg_options,
        need_prelude,
        import_all,
    } = req;

    let gis = dc.get_global_interpreter_state().await.map_err(buck)?;
    let globals = gis.globals().dupe();
    let dialect = StarlarkFileType::Bxl.dialect(gis.disable_starlark_types);
    // Not through the interpreter's own parsing, which names the code after the file and
    // rejects tabs anywhere.
    let ast = AstModule::parse(&format!("<repl:{number}>"), code, &dialect)
        .map_err(ReplFailure::from_starlark)?;
    let load_ids: Vec<String> = ast
        .loads()
        .iter()
        .map(|load| load.module_id.to_owned())
        .collect();

    let (load_paths, import_all_paths, prelude_paths) = {
        let calc = dc
            .get_interpreter_calculator(OwnedStarlarkPath::BxlFile(repl_path.clone()))
            .await
            .map_err(buck)?;
        let mut load_paths = Vec::with_capacity(load_ids.len());
        for id in load_ids {
            let path = calc
                .resolve_load(StarlarkPath::BxlFile(repl_path), &id)
                .await
                .map_err(buck)?;
            load_paths.push((id, path));
        }
        let mut import_all_paths = Vec::with_capacity(import_all.len());
        for id in import_all {
            let path = calc
                .resolve_load(StarlarkPath::BxlFile(repl_path), &id)
                .await
                .map_err(buck)?;
            import_all_paths.push((id, path));
        }
        let prelude_paths = if need_prelude {
            // The implicit imports of an empty `.bxl` file: the prelude, if there is one.
            Some(
                calc.prepare_eval_with_content(StarlarkPath::BxlFile(repl_path), String::new())
                    .and_then(|parsed| parsed)
                    .map(|ParseData(_, imports)| {
                        imports
                            .iter()
                            .filter(|(span, _)| span.is_none())
                            .map(|(_, path)| path.clone())
                            .collect::<Vec<OwnedStarlarkModulePath>>()
                    }),
            )
        } else {
            None
        };
        (load_paths, import_all_paths, prelude_paths)
    };

    let mut loads = HashMap::with_capacity(load_paths.len());
    for (id, path) in load_paths {
        let module = dc.get_loaded_module(path.borrow()).await.map_err(buck)?;
        loads.insert(id, module.env().dupe());
    }
    let mut import_all = Vec::with_capacity(import_all_paths.len());
    for (id, path) in import_all_paths {
        let module = dc.get_loaded_module(path.borrow()).await.map_err(buck)?;
        import_all.push((id, module.env().dupe()));
    }

    let prelude = match prelude_paths {
        None => None,
        Some(Err(e)) => Some(Err(e)),
        Some(Ok(paths)) => {
            let mut modules = Vec::with_capacity(paths.len());
            let mut result = Ok(());
            for path in paths {
                match dc.get_loaded_module(path.borrow()).await {
                    Ok(module) => modules.push(module.env().dupe()),
                    Err(e) => {
                        result = Err(e);
                        break;
                    }
                }
            }
            Some(result.map(|()| modules))
        }
    };

    let key = BxlKey::new(
        BxlFunctionLabel {
            bxl_path: repl_path.clone(),
            name: REPL_FUNCTION_NAME.to_owned(),
        },
        Arc::new(OrderedMap::new()),
        false,
        global_cfg_options,
    );
    let core = Arc::new(
        BxlContextCoreData::new(key, dc)
            .await
            .map_err(buck)?
            .with_repl_cwd(cwd),
    );
    let provider = StarlarkEvaluatorProvider::new(dc, StarlarkEvalKind::Unknown("repl".into()))
        .await
        .map_err(buck)?;
    let digest_config = dc.global_data().get_digest_config();

    Ok(Prepared {
        globals,
        ast,
        loads,
        import_all,
        prelude,
        core,
        provider,
        digest_config,
    })
}
