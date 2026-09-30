/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Completion of names and attributes, on the session thread.
//!
//! No code runs (INV-1): names come from the module's bindings and the globals; a chain of
//! attributes and calls (`ctx.cquery().de`) is walked on live values as long as it has only
//! attributes (which cannot reach DICE: they are not given an evaluator), and then, through
//! calls, on the types that the documentation gives return values ([`TypeIndex`]).

use std::collections::HashSet;

use buck2_cli_proto::ReplChainStep;
use buck2_cli_proto::ReplComplete;
use buck2_cli_proto::ReplCompletions;
use buck2_cli_proto::repl_candidate;
use buck2_cli_proto::repl_chain_step;
use buck2_cli_proto::repl_complete;
use buck2_cli_proto::repl_completions;
use buck2_interpreter::factory::BuckStarlarkModule;
use buck2_repl_syntax::text::CappedString;
use buck2_repl_syntax::text::after_top_level;
use buck2_repl_syntax::text::split_top_level;
use starlark::docs::DocItem;
use starlark::docs::DocMember;
use starlark::environment::Globals;
use starlark::syntax::ast::Visibility;
use starlark::typing::Ty;
use starlark::values::Heap;
use starlark::values::Value;

use crate::repl::complete::candidates::Candidates;
use crate::repl::complete::candidates::completions_status;
use crate::repl::complete::private::PrivateBindings;
use crate::repl::complete::types::TypeIndex;

/// Longest rendering of the type of a function defined in the session that is read for its
/// return type.
const MAX_FUNCTION_TYPE_BYTES: usize = 4 << 10;

/// Completes a name or an attribute (`req.kind` is `NAME` or `ATTR`).
pub(crate) fn complete_starlark(
    env: &BuckStarlarkModule<'_>,
    private: &PrivateBindings,
    globals: Option<&Globals>,
    types: Option<&TypeIndex>,
    req: &ReplComplete,
) -> ReplCompletions {
    let mut candidates = Candidates::default();
    match req.kind() {
        repl_complete::Kind::Name => names(env, private, globals, &req.prefix, &mut candidates),
        repl_complete::Kind::Attr => attributes(env, private, globals, types, req, &mut candidates),
        _ => {
            return completions_status(
                repl_completions::Status::Error,
                "this kind of completion is not supported",
            );
        }
    }
    candidates.into_completions()
}

/// The value bound to `name` in the session, public or private.
fn binding<'v>(
    env: &BuckStarlarkModule<'v>,
    private: &PrivateBindings,
    visibility: Visibility,
    name: &str,
) -> Option<Value<'v>> {
    match visibility {
        Visibility::Public => env.get(name),
        Visibility::Private => Some(private.get(name)?.add_to_heap(env.heap())),
    }
}

/// Whether `name` is a candidate for `prefix`. Names starting with `_` are private: they are
/// candidates only when the prefix starts with `_` too, except `_` itself (the last value).
fn matches(prefix: &str, name: &str) -> bool {
    name.starts_with(prefix) && (name == "_" || !name.starts_with('_') || prefix.starts_with('_'))
}

/// What a value is, as a candidate.
fn kind_of(v: Value) -> repl_candidate::Kind {
    match v.get_type() {
        "function" | "native_method" => repl_candidate::Kind::Function,
        "namespace" => repl_candidate::Kind::Module,
        "type" => repl_candidate::Kind::Type,
        _ => repl_candidate::Kind::Value,
    }
}

/// Adds `name`, completed as a call (`name(`) if it is a function.
fn add(candidates: &mut Candidates, name: &str, kind: repl_candidate::Kind, detail: &str) {
    let replacement = if kind == repl_candidate::Kind::Function {
        format!("{name}(")
    } else {
        name.to_owned()
    };
    candidates.add(replacement, kind, detail);
}

/// The bindings of the session and the globals.
fn names(
    env: &BuckStarlarkModule<'_>,
    private: &PrivateBindings,
    globals: Option<&Globals>,
    prefix: &str,
    candidates: &mut Candidates,
) {
    let mut bound = HashSet::new();
    for (name, visibility) in env.names_and_visibilities() {
        if !matches(prefix, name) {
            continue;
        }
        match binding(env, private, visibility, name) {
            Some(value) => add(candidates, name, kind_of(value), value.get_type()),
            // A loaded symbol whose origin is not known.
            None if visibility == Visibility::Private => {
                add(candidates, name, repl_candidate::Kind::Value, "")
            }
            // Declared by an input that failed before it assigned it.
            None => continue,
        }
        bound.insert(name);
    }
    if let Some(globals) = globals {
        for name in globals.names() {
            if matches(prefix, name) && !bound.contains(name) {
                match globals.get_ref(name) {
                    Some(value) => {
                        let value = value.value();
                        add(candidates, name, kind_of(value), value.get_type());
                    }
                    None => add(candidates, name, repl_candidate::Kind::Value, ""),
                }
            }
        }
    }
}

/// Where the walk of a chain is.
enum Walk<'v> {
    /// At a value.
    Value(Value<'v>),
    /// At a value of one of these types (the names the [`TypeIndex`] knows them by).
    Types(Vec<String>),
    /// At a function that returns values of these types.
    Function(Vec<String>),
}

/// The names of the types of `ty`: one for each alternative of a union. None for `Any`.
fn type_names(ty: &Ty) -> Vec<String> {
    ty.iter_union()
        .iter()
        .filter_map(|t| t.as_name())
        .map(str::to_owned)
        .collect()
}

/// The attributes of the value a chain of attributes and calls ends at.
fn attributes<'v>(
    env: &BuckStarlarkModule<'v>,
    private: &PrivateBindings,
    globals: Option<&Globals>,
    types: Option<&TypeIndex>,
    req: &ReplComplete,
    candidates: &mut Candidates,
) {
    let heap = env.heap();
    let visibility = env
        .names_and_visibilities()
        .find(|(name, _)| *name == req.root)
        .map(|(_, visibility)| visibility);
    let root = match visibility.and_then(|visibility| binding(env, private, visibility, &req.root))
    {
        Some(v) => v,
        None => match globals.and_then(|g| g.get_ref(&req.root)) {
            Some(v) => v.add_to_heap(heap),
            None => return,
        },
    };
    let mut at = Walk::Value(root);
    for step in &req.steps {
        let Some(next) = walk(heap, types, at, step) else {
            return;
        };
        at = next;
    }
    let prefix = &req.prefix;
    match at {
        Walk::Value(v) => {
            let type_name = v.get_type();
            // Sorted: the first candidates are the ones kept.
            for name in v.dir_attr() {
                if !matches(prefix, &name) {
                    continue;
                }
                if candidates.is_full() {
                    candidates.mark_truncated();
                    break;
                }
                match types.and_then(|t| t.member(type_name, &name)) {
                    Some(DocMember::Function(_)) => {
                        add(candidates, &name, repl_candidate::Kind::Function, "")
                    }
                    Some(DocMember::Property(p)) => {
                        let detail = p.typ.as_name().unwrap_or("");
                        add(candidates, &name, repl_candidate::Kind::Value, detail)
                    }
                    // A field of a struct, a record, a provider, ...: attributes cannot run code
                    // that matters (they are not given an evaluator).
                    None => match v.get_attr(&name, heap) {
                        Ok(Some(field)) => add(candidates, &name, kind_of(field), field.get_type()),
                        _ => add(candidates, &name, repl_candidate::Kind::Value, ""),
                    },
                }
            }
        }
        Walk::Types(names) => {
            let Some(types) = types else {
                return;
            };
            for type_name in names {
                let Some(ty) = types.get(&type_name) else {
                    continue;
                };
                for (name, member) in ty.members.iter() {
                    if !matches(prefix, name) {
                        continue;
                    }
                    match member {
                        DocMember::Function(_) => {
                            add(candidates, name, repl_candidate::Kind::Function, "")
                        }
                        DocMember::Property(p) => add(
                            candidates,
                            name,
                            repl_candidate::Kind::Value,
                            p.typ.as_name().unwrap_or(""),
                        ),
                    }
                }
            }
        }
        // Functions have no attributes worth completing.
        Walk::Function(_) => {}
    }
}

/// One step of the walk. `None` when the walk cannot go on.
fn walk<'v>(
    heap: Heap<'v>,
    types: Option<&TypeIndex>,
    at: Walk<'v>,
    step: &ReplChainStep,
) -> Option<Walk<'v>> {
    match (at, step.step.as_ref()?) {
        // A live attribute: attributes are not given an evaluator, so they cannot reach DICE.
        (Walk::Value(v), repl_chain_step::Step::Attr(attr)) => {
            Some(Walk::Value(v.get_attr(attr, heap).ok()??))
        }
        // Calls are never made: the result is typed by the documentation of the function.
        (Walk::Value(v), repl_chain_step::Step::Call(_)) => Some(Walk::Types(returns(v)?)),
        (Walk::Types(names), repl_chain_step::Step::Attr(attr)) => {
            let types = types?;
            let member = names.iter().find_map(|name| types.member(name, attr))?;
            Some(match member {
                DocMember::Property(p) => Walk::Types(type_names(&p.typ)),
                DocMember::Function(f) => Walk::Function(type_names(&f.ret.typ)),
            })
        }
        (Walk::Function(returns), repl_chain_step::Step::Call(_)) => Some(Walk::Types(returns)),
        (Walk::Types(_), repl_chain_step::Step::Call(_))
        | (Walk::Function(_), repl_chain_step::Step::Attr(_)) => None,
    }
}

/// The names of the types of what calling `v` returns, from its documentation (or, for a
/// function defined in the session, from its type).
fn returns(v: Value) -> Option<Vec<String>> {
    match v.get_type() {
        "function" if !v.is_frozen() && Ty::of_value(v).as_function().is_some() => {
            // A `def` (or a `lambda`) defined in the session. Its documentation would format
            // the default values of its parameters, which may be huge or too deep for the
            // stack (as for `:doc`); its type has the return type.
            def_returns(v)
        }
        "function" | "native_method" | "type" => match v.documentation() {
            DocItem::Member(DocMember::Function(f)) => Some(type_names(&f.ret.typ)),
            DocItem::Type(t) => Some(type_names(&t.ty)),
            _ => None,
        },
        _ => None,
    }
}

/// The return types of a function defined in the session, read from its type
/// (`def(x: int) -> bxl.CqueryContext | None`): the type of a function is not public otherwise.
fn def_returns(v: Value) -> Option<Vec<String>> {
    let mut rendered = CappedString::new(MAX_FUNCTION_TYPE_BYTES);
    // The type shows default values as `...`, so it is small.
    std::fmt::write(&mut rendered, format_args!("{}", Ty::of_value(v))).ok()?;
    if rendered.truncated() {
        return None;
    }
    let rendered = rendered.into_string();
    let ret = after_top_level(&rendered, " -> ")?;
    Some(
        split_top_level(ret, " | ")
            .into_iter()
            .map(|t| {
                // `list[str]` is a `list`.
                t.split('[').next().unwrap_or(t).trim().to_owned()
            })
            .filter(|t| !t.is_empty() && t != "typing.Any")
            .collect(),
    )
}
