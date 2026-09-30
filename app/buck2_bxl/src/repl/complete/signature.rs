/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Signatures of functions (`(x: int, *, y: str = "a") -> list[str]`), sent as the `detail` of
//! their completion candidates: the line editor shows the one of the call the cursor is in.
//!
//! Like the rest of completion, this runs no code and formats no value: a `def` or a `lambda`
//! (defined in the session or loaded) is read from the rendering of its type, which shows its
//! default values as `...` (its documentation would format them with `repr`, without bound);
//! other functions from their documentation, which is made when they are registered.

use std::fmt::Write;

use buck2_repl_syntax::signature::Param;
use buck2_repl_syntax::signature::format_signature;
use buck2_repl_syntax::signature::from_def_type;
use buck2_repl_syntax::text::CappedString;
use buck2_repl_syntax::text::truncate_to_bytes;
use starlark::docs::DocFunction;
use starlark::docs::DocItem;
use starlark::docs::DocMember;
use starlark::docs::DocParam;
use starlark::typing::Ty;
use starlark::values::Value;

/// Longest signature sent; a longer one is cut (with `…`).
const MAX_SIGNATURE_BYTES: usize = 400;

/// Longest rendering of the type of a `def` or a `lambda` that is read.
const MAX_FUNCTION_TYPE_BYTES: usize = 4 << 10;

/// The signature of `v`, a value of type `function` or `native_method`.
pub(crate) fn of_function(v: Value) -> Option<String> {
    if v.parameters_spec().is_some() {
        // A `def` or a `lambda`: see the module's documentation.
        let mut rendered = CappedString::new(MAX_FUNCTION_TYPE_BYTES);
        write!(rendered, "{}", Ty::of_value(v)).ok()?;
        if rendered.truncated() {
            return None;
        }
        return from_def_type(rendered.as_str()).map(capped);
    }
    // Native functions, bound methods and partials: their documentation formats no value.
    match v.documentation() {
        DocItem::Member(DocMember::Function(f)) => Some(of_doc(&f)),
        DocItem::Type(t) => Some(of_doc(t.constructor.as_ref()?)),
        _ => None,
    }
}

/// The signature of a documented function.
pub(crate) fn of_doc(f: &DocFunction) -> String {
    let p = &f.params;
    let mut params = Vec::new();
    params.extend(p.pos_only.iter().map(|x| param(x, "")));
    if !p.pos_only.is_empty() {
        params.push(Param::named("/"));
    }
    params.extend(p.pos_or_named.iter().map(|x| param(x, "")));
    match &p.args {
        Some(args) => params.push(param(args, "*")),
        None if !p.named_only.is_empty() => params.push(Param::named("*")),
        None => {}
    }
    params.extend(p.named_only.iter().map(|x| param(x, "")));
    if let Some(kwargs) = &p.kwargs {
        params.push(param(kwargs, "**"));
    }
    let ret = ty(&f.ret.typ);
    capped(format_signature(&params, ret.as_deref()))
}

fn param(p: &DocParam, stars: &str) -> Param {
    Param {
        name: format!("{stars}{}", p.name),
        ty: ty(&p.typ),
        default: p
            .default_value
            .as_ref()
            .map(|d| truncate_to_bytes(d, MAX_SIGNATURE_BYTES).to_owned()),
    }
}

/// The rendering of `t` (at most [`MAX_SIGNATURE_BYTES`]), unless it says nothing.
fn ty(t: &Ty) -> Option<String> {
    if *t == Ty::any() {
        return None;
    }
    let mut rendered = CappedString::new(MAX_SIGNATURE_BYTES);
    write!(rendered, "{t}").ok()?;
    Some(rendered.into_string())
}

/// `signature`, cut to [`MAX_SIGNATURE_BYTES`].
fn capped(signature: String) -> String {
    if signature.len() <= MAX_SIGNATURE_BYTES {
        return signature;
    }
    let mut cut = truncate_to_bytes(&signature, MAX_SIGNATURE_BYTES - '…'.len_utf8()).to_owned();
    cut.push('…');
    cut
}
