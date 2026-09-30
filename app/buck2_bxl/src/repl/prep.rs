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
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use buck2_build_api::bxl::types::BxlFunctionLabel;
use buck2_cli_proto::repl_error;
use buck2_common::dice::cells::HasCellResolver;
use buck2_common::dice::data::HasIoProvider;
use buck2_core::bxl::BxlFilePath;
use buck2_core::cells::CellResolver;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::global_cfg_options::GlobalCfgOptions;
use buck2_execute::digest_config::DigestConfig;
use buck2_execute::digest_config::HasDigestConfig;
use buck2_fs::paths::abs_path::AbsPath;
use buck2_interpreter::dice::starlark_provider::StarlarkEvalKind;
use buck2_interpreter::factory::StarlarkEvaluatorProvider;
use buck2_interpreter::file_loader::LoadedModule;
use buck2_interpreter::file_type::StarlarkFileType;
use buck2_interpreter::load_module::InterpreterCalculation;
use buck2_interpreter::paths::module::OwnedStarlarkModulePath;
use buck2_interpreter::paths::path::OwnedStarlarkPath;
use buck2_interpreter::paths::path::StarlarkPath;
use buck2_interpreter_for_build::interpreter::dice_calculation_delegate::HasCalculationDelegate;
use buck2_interpreter_for_build::interpreter::global_interpreter_state::HasGlobalInterpreterState;
use buck2_interpreter_for_build::interpreter::interpreter_for_dir::ParseData;
use buck2_repl_syntax::text::is_filesystem_path;
use dice::DiceComputations;
use dupe::Dupe;
use dupe::IterDupedExt;
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
    /// A file that was edited (an absolute path): whether the modules loaded load it is told
    /// ([`Prepared::edited_loaded`]).
    pub(crate) edited: Option<String>,
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
    /// For [`PrepRequest::edited`]: whether the file is one of the modules loaded (or imported),
    /// or is loaded by one of them.
    pub(crate) edited_loaded: Option<bool>,
}

/// The name of the code of input `number` (in errors, and in the names of the functions it
/// defines: `<repl:3>.f`).
pub(crate) fn input_file_name(number: u32) -> String {
    format!("<repl:{number}>")
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
        edited,
    } = req;

    let gis = dc.get_global_interpreter_state().await.map_err(buck)?;
    let globals = gis.globals().dupe();
    let dialect = StarlarkFileType::Bxl.dialect(gis.disable_starlark_types);
    // Not through the interpreter's own parsing, which names the code after the file and
    // rejects tabs anywhere.
    let ast = AstModule::parse(&input_file_name(number), code, &dialect)
        .map_err(ReplFailure::from_starlark)?;
    let load_ids: Vec<String> = ast
        .loads()
        .iter()
        .map(|load| load.module_id.to_owned())
        .collect();

    // Modules named by paths of the filesystem that `load()` does not take (`../x.bzl`, an
    // absolute path) are loaded by their labels, and the edited file is found by its cell path.
    let (labels, edited) = {
        let cell_resolver = dc.get_cell_resolver().await.map_err(buck)?;
        let project_root = dc.global_data().get_io_provider().project_root().dupe();
        let files = Files {
            cwd: &cwd,
            cell_resolver,
            project_root: &project_root,
        };
        let mut labels = HashMap::new();
        for id in load_ids.iter().chain(&import_all) {
            if is_filesystem_path(id) {
                let file = files.cell_path(id).map_err(buck)?;
                labels.insert(id.clone(), module_label(&file, &cwd));
            }
        }
        // A file outside the project is loaded by no module.
        let edited = edited.map(|path| files.cell_path(&path).ok());
        (labels, edited)
    };
    let label = |id: &String| labels.get(id).cloned().unwrap_or_else(|| id.clone());

    let (load_paths, import_all_paths, prelude_paths) = {
        let calc = dc
            .get_interpreter_calculator(OwnedStarlarkPath::BxlFile(repl_path.clone()))
            .await
            .map_err(buck)?;
        let mut load_paths = Vec::with_capacity(load_ids.len());
        for id in load_ids {
            let path = calc
                .resolve_load(StarlarkPath::BxlFile(repl_path), &label(&id))
                .await
                .map_err(buck)?;
            load_paths.push((id, path));
        }
        let mut import_all_paths = Vec::with_capacity(import_all.len());
        for id in import_all {
            let path = calc
                .resolve_load(StarlarkPath::BxlFile(repl_path), &label(&id))
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

    let mut loaded_modules = Vec::new();
    let mut loads = HashMap::with_capacity(load_paths.len());
    for (id, path) in load_paths {
        let module = dc.get_loaded_module(path.borrow()).await.map_err(buck)?;
        loads.insert(id, module.env().dupe());
        loaded_modules.push(module.dupe());
    }
    let mut import_all = Vec::with_capacity(import_all_paths.len());
    for (id, path) in import_all_paths {
        let module = dc.get_loaded_module(path.borrow()).await.map_err(buck)?;
        import_all.push((id, module.env().dupe()));
        loaded_modules.push(module.dupe());
    }
    let edited_loaded =
        edited.map(|file| file.is_some_and(|file| loads_file(loaded_modules, &file)));

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
        edited_loaded,
    })
}

/// Turns paths of the filesystem into cell paths.
struct Files<'a> {
    /// The session's working directory, which relative paths are relative to.
    cwd: &'a CellPath,
    cell_resolver: &'a CellResolver,
    project_root: &'a ProjectRoot,
}

impl Files<'_> {
    /// The cell path of the file at `path`: absolute, or relative to the working directory.
    fn cell_path(&self, path: &str) -> buck2_error::Result<CellPath> {
        let path = Path::new(path);
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            let cwd = self.cell_resolver.resolve_path(self.cwd.as_ref())?;
            self.project_root.resolve(&cwd).as_path().join(path)
        };
        let relative = self.project_root.relativize_any(AbsPath::new(&absolute)?)?;
        Ok(self.cell_resolver.get_cell_path(&relative))
    }
}

/// The label `load()` takes for the module `file`: `//dir:file.bzl` in the cell of the working
/// directory, `@cell//dir:file.bzl` in another.
fn module_label(file: &CellPath, cwd: &CellPath) -> String {
    let dir = file.path().parent().map_or("", |dir| dir.as_str());
    let name = file.path().file_name().map_or("", |name| name.as_str());
    if file.cell() == cwd.cell() {
        format!("//{dir}:{name}")
    } else {
        format!("@{}//{dir}:{name}", file.cell())
    }
}

/// Whether one of the `modules`, or a module they load (directly or not), is the file `file`.
fn loads_file(modules: Vec<LoadedModule>, file: &CellPath) -> bool {
    let mut seen: HashSet<CellPath> = HashSet::new();
    let mut stack = modules;
    while let Some(module) = stack.pop() {
        let path = module.path().path().clone();
        if &path == file {
            return true;
        }
        if seen.insert(path) {
            stack.extend(module.loaded_modules().map.values().duped());
        }
    }
    false
}
