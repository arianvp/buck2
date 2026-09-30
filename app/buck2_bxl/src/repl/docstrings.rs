/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! [`Docstrings`]: the docstrings of the functions defined in the session.
//!
//! `:doc` does not ask a function defined in the session for its documentation, which formats
//! the default values of its parameters with no bound on their size or depth (see
//! `render::documentation`). It shows the function's type instead, with the docstring recorded
//! here when the code that defines the function was evaluated.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use starlark::syntax::AstModule;
use starlark::syntax::ast::AstLiteral;
use starlark::syntax::ast::AstStmt;
use starlark::syntax::ast::ExprP;
use starlark::syntax::ast::StmtP;

/// Docstrings by the name a function's value shows: `<repl:3>.f` for a function `f` defined in
/// input 3 (nested functions included). `None` when the function has no docstring, or when the
/// input defines several functions of that name, whose values cannot be told apart.
#[derive(Default)]
pub(crate) struct Docstrings(HashMap<String, Option<String>>);

impl Docstrings {
    /// Records the docstrings of the functions that `ast`, the code of the file (input) named
    /// `file`, defines.
    pub(crate) fn record(&mut self, ast: &AstModule, file: &str) {
        let mut defs = Vec::new();
        collect_defs(ast.statement(), &mut defs);
        let mut input: HashMap<String, Option<String>> = HashMap::new();
        for (name, docstring) in defs {
            match input.entry(format!("{file}.{name}")) {
                Entry::Vacant(e) => {
                    e.insert(docstring);
                }
                Entry::Occupied(mut e) => {
                    e.insert(None);
                }
            }
        }
        self.0.extend(input);
    }

    /// The docstring of the function whose value shows as `name`.
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name)?.as_deref()
    }
}

/// The name and docstring of every `def` in `stmt`. Recursive: the precheck bounds how deeply
/// the statements of an input nest.
fn collect_defs(stmt: &AstStmt, defs: &mut Vec<(String, Option<String>)>) {
    if let StmtP::Def(def) = &stmt.node {
        defs.push((def.name.ident.clone(), docstring(&def.body)));
    }
    stmt.visit_stmt(|child| collect_defs(child, defs));
}

/// The docstring of a function body: its first statement, if it is a string literal (as
/// Starlark finds it).
fn docstring(body: &AstStmt) -> Option<String> {
    let StmtP::Statements(stmts) = &body.node else {
        return None;
    };
    match &stmts.first()?.node {
        StmtP::Expression(expr) => match &expr.node {
            ExprP::Literal(AstLiteral::String(s)) => Some(s.node.clone()),
            _ => None,
        },
        _ => None,
    }
}
