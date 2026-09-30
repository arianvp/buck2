/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! [`TypeIndex`]: the documentation of every type the globals know, by name.
//!
//! The documentation of a value that is not itself a type or a function (an instance, e.g.
//! `ctx`) only names its type. The index finds the type's documentation, with its members, from
//! that name. It is built from the documentation of the globals alone: no code runs.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use starlark::docs::DocItem;
use starlark::docs::DocMember;
use starlark::docs::DocModule;
use starlark::docs::DocType;
use starlark::environment::Globals;

/// Namespaces nest only a few levels deep in the globals; this only bounds the walk.
const MAX_MODULE_DEPTH: usize = 16;

pub(crate) struct TypeIndex {
    types: HashMap<String, DocType>,
}

impl TypeIndex {
    pub(crate) fn build(globals: &Globals) -> Self {
        let mut index = TypeIndex {
            types: HashMap::new(),
        };
        index.add_module("", globals.documentation(), 0);
        index
    }

    /// Adds the types of `module`, whose members are named `{path}.{member}`.
    fn add_module(&mut self, path: &str, module: DocModule, depth: usize) {
        if depth > MAX_MODULE_DEPTH {
            return;
        }
        for (name, item) in module.members {
            let path = if path.is_empty() {
                name
            } else {
                format!("{path}.{name}")
            };
            match item {
                DocItem::Module(module) => self.add_module(&path, module, depth + 1),
                DocItem::Type(ty) => {
                    // By the name values of the type report (`bxl.Context`), and by the path it
                    // is reachable by from the globals, when they differ.
                    if let Some(name) = ty.ty.as_name()
                        && name != path
                    {
                        self.add_type(name.to_owned(), ty.clone());
                    }
                    self.add_type(path, ty);
                }
                DocItem::Member(_) => {}
            }
        }
    }

    /// Several types may report the same name (`target_set` is the name of the sets of
    /// configured and of unconfigured targets): their members are merged.
    fn add_type(&mut self, key: String, ty: DocType) {
        match self.types.entry(key) {
            Entry::Vacant(e) => {
                e.insert(ty);
            }
            Entry::Occupied(mut e) => {
                let known = e.get_mut();
                for (name, member) in ty.members {
                    if !known.members.contains_key(&name) {
                        known.members.insert(name, member);
                    }
                }
                if known.docs.is_none() {
                    known.docs = ty.docs;
                }
                if known.constructor.is_none() {
                    known.constructor = ty.constructor;
                }
            }
        }
    }

    /// The documentation of the type named `name` (`bxl.Context`, `str`, ...).
    pub(crate) fn get(&self, name: &str) -> Option<&DocType> {
        self.types.get(name)
    }

    /// The member `member` of the type named `type_name`.
    pub(crate) fn member(&self, type_name: &str, member: &str) -> Option<&DocMember> {
        self.types.get(type_name)?.members.get(member)
    }
}
