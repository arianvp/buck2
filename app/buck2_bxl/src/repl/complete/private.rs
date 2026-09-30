/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! [`PrivateBindings`]: where the session's private bindings come from.
//!
//! Loaded symbols (by `load`, `:load`, and the prelude) are private bindings of the session's
//! module, whose values the module does not give out (`Module::get` sees public bindings only,
//! and `Evaluator::local_variables` works only while code runs). Completion needs them (to
//! complete `double` as `double(`, or the attributes of a loaded value), so the session records
//! the modules they come from.

use std::collections::HashMap;

use dupe::Dupe;
use starlark::environment::FrozenModule;
use starlark::values::OwnedFrozen;
use starlark::values::Value;

#[derive(Default)]
pub(crate) struct PrivateBindings {
    /// Symbols loaded by name: the local name, and the module and name it was loaded from.
    loaded: HashMap<String, (FrozenModule, String)>,
    /// Modules whose public symbols were all imported (the prelude, `:load` without symbols),
    /// by the string they were loaded with; the latest last.
    imported: Vec<(String, FrozenModule)>,
}

impl PrivateBindings {
    /// `load(<module>, local = "symbol")`.
    pub(crate) fn load(&mut self, local: &str, module: &FrozenModule, symbol: &str) {
        self.loaded
            .insert(local.to_owned(), (module.dupe(), symbol.to_owned()));
    }

    /// Every public symbol of `module` was imported.
    pub(crate) fn import_all(&mut self, id: &str, module: &FrozenModule) {
        // The names it binds are no longer the ones loaded by name.
        for name in module.names() {
            self.loaded.remove(name);
        }
        self.imported.retain(|(known, _)| known != id);
        self.imported.push((id.to_owned(), module.dupe()));
    }

    /// The value of the private binding `name`, if it is known.
    pub(crate) fn get(&self, name: &str) -> Option<OwnedFrozen<Value<'static>>> {
        if let Some((module, symbol)) = self.loaded.get(name) {
            return module.get_any_visibility(symbol).ok().map(|(v, _)| v);
        }
        self.imported
            .iter()
            .rev()
            .find_map(|(_, module)| module.get_any_visibility(name).ok().map(|(v, _)| v))
    }
}
