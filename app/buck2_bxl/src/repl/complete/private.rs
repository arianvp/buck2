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

/// The string the `i`th module of the prelude is recorded with ([`PrivateBindings::import_all`]).
pub(crate) fn prelude_id(i: usize) -> String {
    format!("<prelude {i}>")
}

/// Whether a module was recorded with [`prelude_id`].
pub(crate) fn is_prelude_id(id: &str) -> bool {
    id.starts_with("<prelude ")
}

#[derive(Default)]
pub(crate) struct PrivateBindings {
    /// Symbols loaded by name: the local name, and the module (with the string it was loaded
    /// with) and name it was loaded from.
    loaded: HashMap<String, (String, FrozenModule, String)>,
    /// Modules whose public symbols were all imported (the prelude, `:load` without symbols),
    /// by the string they were loaded with; the latest last.
    imported: Vec<(String, FrozenModule)>,
}

impl PrivateBindings {
    /// The value `load` (and `Module::import_public_symbols`) bind to `symbol` from `module`:
    /// only symbols it exports, whose names do not start with `_`. `None` if a `load` of it
    /// fails.
    pub(crate) fn exported(
        module: &FrozenModule,
        symbol: &str,
    ) -> Option<OwnedFrozen<Value<'static>>> {
        if symbol.starts_with('_') {
            return None;
        }
        module.get(symbol).ok()
    }

    /// `load(<module>, local = "symbol")` bound `local`: the caller checked that `symbol` is
    /// [`exported`](Self::exported).
    pub(crate) fn load(&mut self, local: &str, id: &str, module: &FrozenModule, symbol: &str) {
        self.loaded.insert(
            local.to_owned(),
            (id.to_owned(), module.dupe(), symbol.to_owned()),
        );
    }

    /// Every public symbol of `module` was imported.
    pub(crate) fn import_all(&mut self, id: &str, module: &FrozenModule) {
        // The names it binds are no longer the ones loaded by name.
        for name in module.names() {
            if !name.starts_with('_') {
                self.loaded.remove(name);
            }
        }
        self.imported.retain(|(known, _)| known != id);
        self.imported.push((id.to_owned(), module.dupe()));
    }

    /// The value of the private binding `name`, if it is known.
    pub(crate) fn get(&self, name: &str) -> Option<OwnedFrozen<Value<'static>>> {
        self.find(name).map(|(_, value)| value)
    }

    /// The value of the private binding `name` and the string its module was loaded with (as
    /// `import_all` was given it for the prelude), if they are known.
    pub(crate) fn find(&self, name: &str) -> Option<(&str, OwnedFrozen<Value<'static>>)> {
        if let Some((id, module, symbol)) = self.loaded.get(name) {
            return Self::exported(module, symbol).map(|value| (id.as_str(), value));
        }
        // Only what the import bound: a module imported later that has a private binding of
        // the same name (a symbol it loaded itself) did not rebind it.
        self.imported
            .iter()
            .rev()
            .find_map(|(id, module)| Self::exported(module, name).map(|value| (id.as_str(), value)))
    }
}
