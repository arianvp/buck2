/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Completion of names, attributes and keyword arguments, on the session thread.
//!
//! No code runs (INV-1): names come from the module's bindings and the globals; a chain of
//! attributes and calls (`ctx.cquery().de`) is walked on live values as long as it has only
//! attributes (which cannot reach DICE: they are not given an evaluator), and then, through
//! calls, on the types that the documentation gives return values ([`TypeIndex`]). The keyword
//! arguments of a call (`ctx.configured_targets(tar`) are the parameters of the function the
//! chain ends at: from its documentation, or, for a `def` or a `lambda`, its parameters spec.

use std::collections::HashSet;

use buck2_cli_proto::ReplChainStep;
use buck2_cli_proto::ReplComplete;
use buck2_cli_proto::ReplCompletions;
use buck2_cli_proto::repl_candidate;
use buck2_cli_proto::repl_chain_step;
use buck2_cli_proto::repl_complete;
use buck2_cli_proto::repl_completions;
use buck2_interpreter::factory::BuckStarlarkModule;
use buck2_repl_syntax::lexer::is_ident_continue;
use buck2_repl_syntax::lexer::is_ident_start;
use buck2_repl_syntax::matching::match_tier;
use buck2_repl_syntax::text::CappedString;
use buck2_repl_syntax::text::after_top_level;
use buck2_repl_syntax::text::split_top_level;
use starlark::docs::DocFunction;
use starlark::docs::DocItem;
use starlark::docs::DocMember;
use starlark::docs::DocParams;
use starlark::environment::Globals;
use starlark::syntax::ast::Visibility;
use starlark::typing::Ty;
use starlark::values::Heap;
use starlark::values::Value;

use crate::repl::complete::candidates::Candidates;
use crate::repl::complete::candidates::completions_status;
use crate::repl::complete::private::PrivateBindings;
use crate::repl::complete::signature;
use crate::repl::complete::types::TypeIndex;

/// Longest rendering of the type of a `def` or a `lambda` that is read for its return type.
const MAX_FUNCTION_TYPE_BYTES: usize = 4 << 10;

/// Most parameters of a `def` or a `lambda` offered as keyword arguments.
const MAX_PARAMETERS: usize = 1024;

/// Completes a name, an attribute, or an argument of a call (`req.kind` is `NAME`, `ATTR` or
/// `KWARG`).
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
        repl_complete::Kind::Kwarg => {
            keyword_arguments(env, private, globals, types, req, &mut candidates);
            names(env, private, globals, &req.prefix, &mut candidates);
        }
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

/// Whether `name` may be a candidate for `prefix`. Names starting with `_` are private: they
/// are candidates only when the prefix starts with `_` too, except `_` itself (the last value).
fn visible(prefix: &str, name: &str) -> bool {
    name == "_" || !name.starts_with('_') || prefix.starts_with('_')
}

/// What a value is, as a candidate.
pub(crate) fn kind_of(v: Value) -> repl_candidate::Kind {
    match v.get_type() {
        "function" | "native_method" => repl_candidate::Kind::Function,
        "namespace" => repl_candidate::Kind::Module,
        "type" => repl_candidate::Kind::Type,
        _ => repl_candidate::Kind::Value,
    }
}

/// The detail of the candidate for the value `v`: its signature if it is a function (and the
/// answer takes more signatures), else its type.
fn detail_of(candidates: &mut Candidates, v: Value) -> String {
    if kind_of(v) == repl_candidate::Kind::Function
        && candidates.wants_signature()
        && let Some(signature) = signature::of_function(v)
    {
        return signature;
    }
    v.get_type().to_owned()
}

/// The detail of the candidate for a function documented by `f`: its signature, if the answer
/// takes more signatures.
fn doc_detail(candidates: &mut Candidates, f: &DocFunction) -> String {
    if candidates.wants_signature() {
        signature::of_doc(f)
    } else {
        String::new()
    }
}

/// Offers `name` for `prefix`, completed as a call (`name(`) if it is a function.
fn add(
    candidates: &mut Candidates,
    prefix: &str,
    name: &str,
    kind: repl_candidate::Kind,
    detail: &str,
) {
    let replacement = if kind == repl_candidate::Kind::Function {
        format!("{name}(")
    } else {
        name.to_owned()
    };
    candidates.offer(prefix, name, replacement, kind, detail);
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
        if !visible(prefix, name) || match_tier(prefix, name).is_none() {
            continue;
        }
        match binding(env, private, visibility, name) {
            Some(value) => {
                let detail = detail_of(candidates, value);
                add(candidates, prefix, name, kind_of(value), &detail)
            }
            // Defined in the session with a private name (`_x = 1`): the module does not give
            // out its value.
            None if visibility == Visibility::Private && name.starts_with('_') => {
                add(candidates, prefix, name, repl_candidate::Kind::Value, "")
            }
            // Declared by an input that failed before it assigned it, or by a `load` whose
            // symbol does not exist.
            None => continue,
        }
        bound.insert(name);
    }
    if let Some(globals) = globals {
        for name in globals.names() {
            if visible(prefix, name) && match_tier(prefix, name).is_some() && !bound.contains(name)
            {
                match globals.get_ref(name) {
                    Some(value) => {
                        let value = value.value();
                        let detail = detail_of(candidates, value);
                        add(candidates, prefix, name, kind_of(value), &detail);
                    }
                    None => add(candidates, prefix, name, repl_candidate::Kind::Value, ""),
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
    /// At a function that returns values of these types, and takes these parameters.
    Function(Vec<String>, Box<DocParams>),
}

/// The names of the types of `ty`: one for each alternative of a union. None for `Any`.
fn type_names(ty: &Ty) -> Vec<String> {
    ty.iter_union()
        .iter()
        .filter_map(|t| t.as_name())
        .map(str::to_owned)
        .collect()
}

/// Where the chain `root` + `steps` ends: `None` if it cannot be followed.
fn resolve<'v>(
    env: &BuckStarlarkModule<'v>,
    private: &PrivateBindings,
    globals: Option<&Globals>,
    types: Option<&TypeIndex>,
    root: &str,
    steps: &[ReplChainStep],
) -> Option<Walk<'v>> {
    let heap = env.heap();
    let visibility = env
        .names_and_visibilities()
        .find(|(name, _)| *name == root)
        .map(|(_, visibility)| visibility);
    let root = match visibility.and_then(|visibility| binding(env, private, visibility, root)) {
        Some(v) => v,
        None => globals.and_then(|g| g.get_ref(root))?.add_to_heap(heap),
    };
    let mut at = Walk::Value(root);
    for step in steps {
        at = walk(heap, types, at, step)?;
    }
    Some(at)
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
    let Some(at) = resolve(env, private, globals, types, &req.root, &req.steps) else {
        return;
    };
    let prefix = &req.prefix;
    match at {
        Walk::Value(v) => {
            let type_name = v.get_type();
            // Sorted: the first candidates are the ones kept.
            for name in v.dir_attr() {
                if !visible(prefix, &name) || match_tier(prefix, &name).is_none() {
                    continue;
                }
                if candidates.is_full() {
                    candidates.mark_truncated();
                    break;
                }
                match types.and_then(|t| t.member(type_name, &name)) {
                    Some(DocMember::Function(f)) => {
                        let detail = doc_detail(candidates, f);
                        add(
                            candidates,
                            prefix,
                            &name,
                            repl_candidate::Kind::Function,
                            &detail,
                        )
                    }
                    Some(DocMember::Property(p)) => {
                        let detail = p.typ.as_name().unwrap_or("");
                        add(
                            candidates,
                            prefix,
                            &name,
                            repl_candidate::Kind::Value,
                            detail,
                        )
                    }
                    // A field of a struct, a record, a provider, ...: attributes cannot run code
                    // that matters (they are not given an evaluator).
                    None => match v.get_attr(&name, heap) {
                        Ok(Some(field)) => {
                            let detail = detail_of(candidates, field);
                            add(candidates, prefix, &name, kind_of(field), &detail)
                        }
                        _ => add(candidates, prefix, &name, repl_candidate::Kind::Value, ""),
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
                    if !visible(prefix, name) || match_tier(prefix, name).is_none() {
                        continue;
                    }
                    match member {
                        DocMember::Function(f) => {
                            let detail = doc_detail(candidates, f);
                            add(
                                candidates,
                                prefix,
                                name,
                                repl_candidate::Kind::Function,
                                &detail,
                            )
                        }
                        DocMember::Property(p) => add(
                            candidates,
                            prefix,
                            name,
                            repl_candidate::Kind::Value,
                            p.typ.as_name().unwrap_or(""),
                        ),
                    }
                }
            }
        }
        // Functions have no attributes worth completing.
        Walk::Function(..) => {}
    }
}

/// A parameter that an argument may name.
struct Parameter {
    name: String,
    /// For a parameter that may be given by position too: its position (counting the
    /// positional-only parameters before it).
    position: Option<usize>,
}

/// The parameters that may be named in a call, from the documentation of a function.
fn named_parameters(params: &DocParams) -> Vec<Parameter> {
    let first = params.pos_only.len();
    params
        .pos_or_named
        .iter()
        .enumerate()
        .map(|(i, p)| Parameter {
            name: p.name.clone(),
            position: Some(first + i),
        })
        .chain(params.named_only.iter().map(|p| Parameter {
            name: p.name.clone(),
            position: None,
        }))
        .collect()
}

/// The parameters that may be named in a call of `v`, or `None` if it is not a function whose
/// parameters are known.
fn parameters_of(v: Value) -> Option<Vec<Parameter>> {
    if let Some(spec) = v.parameters_spec() {
        // A `def` or a `lambda`: its documentation would format the default values of its
        // parameters without bound (see `returns`); the parameters spec names them.
        return Some(parse_parameters(&spec.parameters_str()));
    }
    match v.get_type() {
        // Native functions and methods, and types (their constructor).
        "function" | "native_method" | "type" => match v.documentation() {
            DocItem::Member(DocMember::Function(f)) => Some(named_parameters(&f.params)),
            DocItem::Type(t) => Some(named_parameters(&t.constructor?.params)),
            _ => None,
        },
        _ => None,
    }
}

/// The parameters that may be named, from the parameters of a `def` as `ParametersSpec`
/// renders them (`a, b = ..., /, c, *args, d, **kwargs`: `/` ends the positional-only
/// parameters, `*` or `*args` starts the named-only ones).
fn parse_parameters(rendered: &str) -> Vec<Parameter> {
    let mut parameters = Vec::new();
    let mut position = 0usize;
    let mut named_only = false;
    for item in rendered.split(", ").take(MAX_PARAMETERS) {
        match item {
            // The parameters before it were positional-only.
            "/" => parameters.clear(),
            _ if item.starts_with("**") => {}
            _ if item.starts_with('*') => named_only = true,
            _ => {
                let name = item.split(" = ").next().unwrap_or("");
                let identifier = name.chars().next().is_some_and(is_ident_start)
                    && name.chars().all(is_ident_continue);
                if identifier {
                    parameters.push(Parameter {
                        name: name.to_owned(),
                        position: (!named_only).then_some(position),
                    });
                }
                position += 1;
            }
        }
    }
    parameters
}

/// The keyword arguments of the call of the chain `req.root` + `req.steps` (`name=`), but those
/// given already (`req.used_kwargs`) and those that the positional arguments given
/// (`req.positional_args`) fill.
fn keyword_arguments<'v>(
    env: &BuckStarlarkModule<'v>,
    private: &PrivateBindings,
    globals: Option<&Globals>,
    types: Option<&TypeIndex>,
    req: &ReplComplete,
    candidates: &mut Candidates,
) {
    let parameters = match resolve(env, private, globals, types, &req.root, &req.steps) {
        Some(Walk::Value(v)) => parameters_of(v),
        Some(Walk::Function(_, params)) => Some(named_parameters(&params)),
        Some(Walk::Types(_)) | None => None,
    };
    let positional = usize::try_from(req.positional_args).unwrap_or(usize::MAX);
    for parameter in parameters.into_iter().flatten() {
        if parameter.position.is_some_and(|p| p < positional)
            || req.used_kwargs.contains(&parameter.name)
            || !visible(&req.prefix, &parameter.name)
        {
            continue;
        }
        candidates.offer(
            &req.prefix,
            &parameter.name,
            format!("{}=", parameter.name),
            repl_candidate::Kind::Kwarg,
            "",
        );
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
                DocMember::Function(f) => {
                    Walk::Function(type_names(&f.ret.typ), Box::new(f.params.clone()))
                }
            })
        }
        (Walk::Function(returns, _), repl_chain_step::Step::Call(_)) => Some(Walk::Types(returns)),
        (Walk::Types(_), repl_chain_step::Step::Call(_))
        | (Walk::Function(..), repl_chain_step::Step::Attr(_)) => None,
    }
}

/// The names of the types of what calling `v` returns, from its documentation (or, for a
/// `def` or a `lambda`, from its type).
fn returns(v: Value) -> Option<Vec<String>> {
    if v.parameters_spec().is_some() {
        // A `def` or a `lambda` (only those have a parameters spec), defined in the session or
        // loaded from a file. Its documentation would format the default values of its
        // parameters with `repr`, without bound on their size or depth (as for `:doc`): a
        // default that shares its parts (`x = [x, x]` 60 times) takes for ever, and one too
        // deep for the stack aborts the daemon. Its type has the return type, and shows default
        // values as `...`.
        return def_returns(v);
    }
    match v.get_type() {
        // Native functions and methods (their documentation is made once, when they are
        // registered), types, and other callables whose documentation formats no value.
        "function" | "native_method" | "type" => match v.documentation() {
            DocItem::Member(DocMember::Function(f)) => Some(type_names(&f.ret.typ)),
            DocItem::Type(t) => Some(type_names(&t.ty)),
            _ => None,
        },
        _ => None,
    }
}

/// The return types of a `def` or a `lambda`, read from its type
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
