/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Signatures of functions, as the line editor's hint shows them while the cursor is in a call:
//! `(x: int, /, y: str = "a", *args, z = ..., **kwargs) -> list[str]`.
//!
//! The daemon sends the signature of each function it offers as a completion candidate (its
//! `detail`); the client keeps them and shows the one of the call the cursor is in, with the
//! parameter that the argument at the cursor fills picked out ([`active_parameter`]).

use std::ops::Range;

use crate::text::split_top_level;

/// The annotation of a parameter or of a return value that says nothing, left out.
const ANY: &str = "typing.Any";

/// Longest type or default value shown in full: a hint must stay short. A longer union keeps
/// its first alternatives (`str | list[str] | …`).
pub const MAX_PART_CHARS: usize = 32;

/// A parameter of a signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    /// The name, with `*` or `**` for `*args` and `**kwargs`; `/` and `*` for the markers of
    /// positional-only and named-only parameters.
    pub name: String,
    /// The type, unless it says nothing (`typing.Any`).
    pub ty: Option<String>,
    /// The default value, as written (`...` when it is not known).
    pub default: Option<String>,
}

impl Param {
    /// A parameter without a type or a default.
    pub fn named(name: impl Into<String>) -> Self {
        Param {
            name: name.into(),
            ty: None,
            default: None,
        }
    }
}

/// `(params) -> ret`, the return type left out when it says nothing. Types and default values
/// longer than [`MAX_PART_CHARS`] are shortened ([`abbreviate_type`], [`abbreviate`]).
pub fn format_signature(params: &[Param], ret: Option<&str>) -> String {
    let mut out = String::from("(");
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&p.name);
        if let Some(ty) = p.ty.as_deref().filter(|ty| *ty != ANY) {
            out.push_str(": ");
            out.push_str(&abbreviate_type(ty));
        }
        if let Some(default) = &p.default {
            out.push_str(if p.ty.as_deref().is_some_and(|ty| ty != ANY) {
                " = "
            } else {
                "="
            });
            out.push_str(&abbreviate(default));
        }
    }
    out.push(')');
    if let Some(ret) = ret.filter(|ret| !ret.is_empty() && *ret != ANY) {
        out.push_str(" -> ");
        out.push_str(&abbreviate_type(ret));
    }
    out
}

/// The type `ty`, or the first alternatives of it (a union) that fit in [`MAX_PART_CHARS`],
/// followed by `| …`; `…` if even the first one does not fit.
pub fn abbreviate_type(ty: &str) -> String {
    if ty.chars().count() <= MAX_PART_CHARS {
        return ty.to_owned();
    }
    let mut kept = String::new();
    for alternative in split_top_level(ty, " | ") {
        let len = kept.chars().count() + alternative.chars().count() + 3;
        if len > MAX_PART_CHARS {
            break;
        }
        kept.push_str(alternative);
        kept.push_str(" | ");
    }
    kept.push('…');
    kept
}

/// `text`, cut to [`MAX_PART_CHARS`] characters with `…`.
pub fn abbreviate(text: &str) -> String {
    if text.chars().count() <= MAX_PART_CHARS {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(MAX_PART_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// The signature of a function from the rendering of its type:
/// `def(x: int, y: typing.Any = ..., *args: typing.Any) -> typing.Any` gives
/// `(x: int, y=..., *args)`. `None` if `rendered` is not the type of a function.
pub fn from_def_type(rendered: &str) -> Option<String> {
    let rest = rendered.strip_prefix("def")?;
    let params_end = closing_paren(rest)?;
    let inner = rest.get(1..params_end)?;
    let ret = rest.get(params_end + 1..)?.trim_start().strip_prefix("->");
    let params: Vec<Param> = split_params(inner)
        .into_iter()
        .filter_map(|range| inner.get(range))
        .map(parse_param)
        .collect();
    Some(format_signature(&params, ret.map(str::trim)))
}

/// Reads one parameter as a type renders it (`name: type = default`).
fn parse_param(text: &str) -> Param {
    let text = text.trim();
    let (head, default) = match split_top(text, '=') {
        Some((head, default)) => (head.trim(), Some(default.trim().to_owned())),
        None => (text, None),
    };
    let (name, ty) = match split_top(head, ':') {
        Some((name, ty)) => (name.trim(), Some(ty.trim().to_owned())),
        None => (head, None),
    };
    Param {
        name: name.to_owned(),
        ty,
        default,
    }
}

/// `text` split at the first `sep` outside brackets and strings.
fn split_top(text: &str, sep: char) -> Option<(&str, &str)> {
    let at = top_level(text).find(|&(_, c)| c == sep)?.0;
    Some((text.get(..at)?, text.get(at + sep.len_utf8()..)?))
}

/// The characters of `text` outside brackets (`()`, `[]`, `{}`) and string literals, with
/// their byte offsets. Brackets themselves are not included.
fn top_level(text: &str) -> impl Iterator<Item = (usize, char)> + '_ {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    text.char_indices().filter(move |&(_, c)| {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            return false;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                false
            }
            '(' | '[' | '{' => {
                depth += 1;
                false
            }
            ')' | ']' | '}' => {
                depth = depth.saturating_sub(1);
                false
            }
            _ => depth == 0,
        }
    })
}

/// The byte offset of the `)` that closes the `(` that `s` starts with.
fn closing_paren(s: &str) -> Option<usize> {
    if !s.starts_with('(') {
        return None;
    }
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The byte ranges of the parameters in `inner` (the text between the parentheses of a
/// signature), trimmed. None for `()`.
fn split_params(inner: &str) -> Vec<Range<usize>> {
    if inner.trim().is_empty() {
        return Vec::new();
    }
    let mut ranges = Vec::new();
    let mut start = 0;
    for (i, _) in top_level(inner).filter(|&(_, c)| c == ',') {
        ranges.push(trimmed(inner, start..i));
        start = i + 1;
    }
    ranges.push(trimmed(inner, start..inner.len()));
    ranges
}

/// `range` of `text` without the whitespace at its ends.
fn trimmed(text: &str, range: Range<usize>) -> Range<usize> {
    let part = text.get(range.clone()).unwrap_or("");
    let start = range.start + (part.len() - part.trim_start().len());
    let end = range.end - (part.len() - part.trim_end().len());
    start..end.max(start)
}

/// The name of the parameter in `param` (`name: type = default`), without stars.
fn param_name(param: &str) -> &str {
    let end = param
        .find(|c: char| c == ':' || c == '=' || c.is_whitespace())
        .unwrap_or(param.len());
    param.get(..end).unwrap_or("")
}

/// The byte range, in `signature` (`(params) -> ret`), of the parameter that an argument of a
/// call fills: the one named `keyword` (or else `**kwargs`) for a keyword argument, else the
/// positional one after `positional` positional arguments (or `*args`). `None` if no parameter
/// takes it.
pub fn active_parameter(
    signature: &str,
    positional: usize,
    keyword: Option<&str>,
) -> Option<Range<usize>> {
    let end = closing_paren(signature)?;
    let inner = signature.get(1..end)?;
    let params: Vec<(Range<usize>, &str)> = split_params(inner)
        .into_iter()
        .filter_map(|r| Some((r.clone(), inner.get(r)?)))
        .collect();
    let found = match keyword {
        Some(keyword) => {
            // A named parameter is one after the positional-only marker `/`.
            let named_from = params
                .iter()
                .position(|(_, p)| *p == "/")
                .map_or(0, |i| i + 1);
            params
                .iter()
                .skip(named_from)
                .find(|(_, p)| !p.starts_with('*') && param_name(p) == keyword)
                .or_else(|| params.iter().find(|(_, p)| p.starts_with("**")))
        }
        None => {
            let mut index = 0usize;
            let mut found = None;
            for param in &params {
                let (_, p) = param;
                if *p == "/" {
                    continue;
                }
                if p.starts_with("**") || *p == "*" {
                    break;
                }
                if p.starts_with('*') {
                    // `*args` takes every positional argument from here on.
                    found = Some(param);
                    break;
                }
                if index == positional {
                    found = Some(param);
                    break;
                }
                index += 1;
            }
            found
        }
    }?;
    let range = found.0.clone();
    Some(range.start + 1..range.end + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_def_type() {
        assert_eq!(
            from_def_type("def(x: int, y: typing.Any = ..., *args: typing.Any) -> typing.Any")
                .as_deref(),
            Some("(x: int, y=..., *args)")
        );
        assert_eq!(
            from_def_type(
                "def(a: def(int) -> str, /, *, b: list[str] = ..., **kwargs: int) -> dict[str, int]"
            )
            .as_deref(),
            Some("(a: def(int) -> str, /, *, b: list[str] = ..., **kwargs: int) -> dict[str, int]")
        );
        assert_eq!(
            from_def_type("def() -> None").as_deref(),
            Some("() -> None")
        );
        assert_eq!(from_def_type("function"), None);
        assert_eq!(from_def_type("def(x"), None);
        assert_eq!(from_def_type("def"), None);
    }

    #[test]
    fn test_format_signature() {
        let params = [
            Param {
                name: "labels".to_owned(),
                ty: Some("str | list[str]".to_owned()),
                default: None,
            },
            Param::named("*"),
            Param {
                name: "target_platform".to_owned(),
                ty: Some(ANY.to_owned()),
                default: Some("None".to_owned()),
            },
            Param {
                name: "modifiers".to_owned(),
                ty: Some("list[str]".to_owned()),
                default: Some("[]".to_owned()),
            },
        ];
        assert_eq!(
            format_signature(&params, Some("bxl.ConfiguredTargetSet")),
            "(labels: str | list[str], *, target_platform=None, modifiers: list[str] = []) -> bxl.ConfiguredTargetSet"
        );
        assert_eq!(format_signature(&[], Some(ANY)), "()");
        assert_eq!(format_signature(&[], None), "()");
    }

    #[test]
    fn test_abbreviate() {
        assert_eq!(abbreviate_type("str | list[str]"), "str | list[str]");
        assert_eq!(
            abbreviate_type("ConfiguredTargetLabel | TargetLabel | bxl.ConfiguredTargetNode | str"),
            "ConfiguredTargetLabel | …"
        );
        assert_eq!(
            abbreviate_type("int | str | list[int | str | None] | dict[str, list[str]]"),
            "int | str | …"
        );
        assert_eq!(
            abbreviate_type("dict[str, list[ConfiguredTargetLabel | TargetLabel]]"),
            "…"
        );
        assert_eq!(abbreviate("\"short\""), "\"short\"");
        let long = "\"".to_owned() + &"é".repeat(40) + "\"";
        let cut = abbreviate(&long);
        assert_eq!(cut.chars().count(), MAX_PART_CHARS);
        assert!(cut.ends_with('…'));
        let params = [Param {
            name: "universe".to_owned(),
            ty: Some("ConfiguredTargetLabel | TargetLabel | bxl.ConfiguredTargetNode".to_owned()),
            default: Some("None".to_owned()),
        }];
        assert_eq!(
            format_signature(
                &params,
                Some("dict[str, list[ConfiguredTargetLabel | TargetLabel]]")
            ),
            "(universe: ConfiguredTargetLabel | … = None) -> …"
        );
    }

    #[test]
    fn test_active_parameter() {
        let sig =
            "(a, b: str = \"x, y\", /, c: dict[str, int] = {1: 2}, *args, d=..., **kw) -> int";
        let active =
            |positional, keyword| active_parameter(sig, positional, keyword).map(|r| &sig[r]);
        assert_eq!(active(0, None), Some("a"));
        assert_eq!(active(1, None), Some("b: str = \"x, y\""));
        assert_eq!(active(2, None), Some("c: dict[str, int] = {1: 2}"));
        assert_eq!(active(3, None), Some("*args"));
        assert_eq!(active(9, None), Some("*args"));
        assert_eq!(active(0, Some("c")), Some("c: dict[str, int] = {1: 2}"));
        assert_eq!(active(0, Some("d")), Some("d=..."));
        // Positional-only parameters cannot be named: `**kw` takes it.
        assert_eq!(active(0, Some("a")), Some("**kw"));
        assert_eq!(active(0, Some("zz")), Some("**kw"));

        let sig = "(x, *, y) -> None";
        assert_eq!(active_parameter(sig, 0, None).map(|r| &sig[r]), Some("x"));
        assert_eq!(active_parameter(sig, 1, None), None);
        assert_eq!(
            active_parameter(sig, 0, Some("y")).map(|r| &sig[r]),
            Some("y")
        );
        assert_eq!(active_parameter(sig, 0, Some("z")), None);
        assert_eq!(active_parameter("()", 0, None), None);
        assert_eq!(active_parameter("(x", 0, None), None);
        assert_eq!(active_parameter("", 0, None), None);
    }
}
