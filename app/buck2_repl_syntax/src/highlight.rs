/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Syntax highlighting for the line editor.
//!
//! [`spans`] says which parts of an input to colour: keywords, strings, numbers, comments and
//! the meta-command token, found with the tolerant [`lexer`](crate::lexer) (so it works on input
//! that does not parse yet), plus the bracket that matches the one at the cursor. The argument
//! of a meta-command is highlighted as what it is: Starlark for `:print`, `:type`, ..., the
//! input it runs for `:time`, a query for `:cquery`, ...; other arguments (targets, paths) are
//! left alone. [`paint`] wraps the spans in ANSI SGR sequences, which take no room on the
//! terminal: the text shown, and so its width, is unchanged, which the line editor relies on
//! to place the cursor.

use crate::commands::ArgKind;
use crate::commands::CommandError;
use crate::commands::resolve_command;
use crate::commands::split_command_token;
use crate::lexer::Bracket;
use crate::lexer::TokenKind;
use crate::lexer::is_reserved;
use crate::lexer::lex;
use crate::query::OPERATOR_WORDS;
use crate::query::is_word_char;

/// What a highlighted part of an input is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    /// A Starlark keyword (`def`, `if`, `load`, ...), or an operator word of a query (`union`).
    Keyword,
    /// A word that Starlark reserves and rejects (`while`, `class`, ...).
    Reserved,
    /// `True`, `False` or `None`.
    Constant,
    /// A number.
    Number,
    /// A string literal, closed or not.
    String,
    /// A comment.
    Comment,
    /// The token of a meta-command (`:build`), with its colon.
    Command,
    /// The token of a meta-command that no command has (`:zz`).
    UnknownCommand,
    /// A function called in a query (`deps` in `deps(//x:y)`).
    Function,
    /// The bracket that matches the one at the cursor.
    MatchingBracket,
    /// A character that cannot start a Starlark token.
    Error,
}

/// A highlighted part of an input: bytes `start..end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub class: Class,
}

/// The SGR sequence that ends a highlighted part.
pub const RESET: &str = "\x1b[0m";

/// The SGR sequence that starts a part of class `class`, in the default palette. Only the basic
/// eight colours, bold, faint and reverse video are used, so that the terminal's theme decides
/// what they look like.
pub fn ansi_style(class: Class) -> &'static str {
    match class {
        Class::Keyword => "\x1b[35m",
        Class::Reserved | Class::UnknownCommand | Class::Error => "\x1b[31m",
        Class::Constant | Class::Number => "\x1b[36m",
        Class::String => "\x1b[32m",
        Class::Comment => "\x1b[2m",
        Class::Command => "\x1b[1;34m",
        Class::Function => "\x1b[34m",
        Class::MatchingBracket => "\x1b[1;7m",
    }
}

/// A bracket of the input (not in a string or a comment).
#[derive(Debug, Clone, Copy)]
struct BracketAt {
    pos: usize,
    bracket: Bracket,
    open: bool,
}

#[derive(Default)]
struct Collector {
    spans: Vec<Span>,
    brackets: Vec<BracketAt>,
}

impl Collector {
    fn span(&mut self, start: usize, end: usize, class: Class) {
        if start < end {
            self.spans.push(Span { start, end, class });
        }
    }

    fn bracket(&mut self, pos: usize, bracket: Bracket, open: bool) {
        self.brackets.push(BracketAt { pos, bracket, open });
    }
}

/// The parts of `buf` to highlight, sorted and not overlapping. With `cursor` (a byte offset),
/// the bracket that matches the bracket at the cursor (or else just before it) is one of them.
pub fn spans(buf: &str, cursor: Option<usize>) -> Vec<Span> {
    let mut collector = Collector::default();
    input(buf, 0, &mut collector);
    let Collector {
        mut spans,
        brackets,
    } = collector;
    if let Some(pos) = cursor.and_then(|cursor| matching_bracket(&brackets, cursor)) {
        spans.push(Span {
            start: pos,
            end: pos + 1,
            class: Class::MatchingBracket,
        });
    }
    spans.sort_by_key(|s| s.start);
    spans
}

/// `buf` highlighted for a terminal: [`spans`] painted with [`ansi_style`]. `None` if nothing is
/// highlighted.
pub fn highlight(buf: &str, cursor: Option<usize>) -> Option<String> {
    let spans = spans(buf, cursor);
    (!spans.is_empty()).then(|| paint(buf, &spans, ansi_style))
}

/// `buf` with each span wrapped in `style(class)` and [`RESET`]. A span that covers a newline is
/// ended before it and started again after it, so that every line of the result stands alone.
/// Only SGR sequences are added: without them the result is `buf`. Spans that are not sorted,
/// overlap, or are not on character boundaries of `buf` are skipped.
pub fn paint(buf: &str, spans: &[Span], style: impl Fn(Class) -> &'static str) -> String {
    let mut out = String::with_capacity(buf.len() + spans.len() * 12);
    let mut at = 0;
    for span in spans {
        if span.start < at {
            continue;
        }
        let (Some(before), Some(text)) = (buf.get(at..span.start), buf.get(span.start..span.end))
        else {
            continue;
        };
        out.push_str(before);
        let sgr = style(span.class);
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                out.push('\n');
            }
            if !line.is_empty() {
                out.push_str(sgr);
                out.push_str(line);
                out.push_str(RESET);
            }
        }
        at = span.end;
    }
    out.push_str(buf.get(at..).unwrap_or(""));
    out
}

/// Highlights `text`, an input (possibly a meta-command) that starts at byte `offset`.
fn input(text: &str, offset: usize, c: &mut Collector) {
    let mut text = text;
    let mut offset = offset;
    loop {
        let Some(token) = split_command_token(text) else {
            starlark(text, offset, c);
            return;
        };
        let Some(colon) = token.token_start.checked_sub(1) else {
            return;
        };
        let spec = resolve_command(token.token);
        let class = match &spec {
            Err(CommandError::Unknown { .. }) => Class::UnknownCommand,
            // A command, or a prefix of several (`:re`), or the colon alone: still being typed.
            _ => Class::Command,
        };
        c.span(offset + colon, offset + token.arg_start, class);
        let (Ok(spec), Some(rest)) = (spec, text.get(token.arg_start..)) else {
            return;
        };
        let arg_offset = offset + token.arg_start;
        match spec.arg {
            // Any input, possibly another command (`:time :b //x`).
            ArgKind::Input => {
                offset = arg_offset;
                text = rest;
            }
            ArgKind::Expr => {
                starlark(rest, arg_offset, c);
                return;
            }
            ArgKind::Query(_) => {
                query(rest, arg_offset, c);
                return;
            }
            // Targets, paths, words: as typed.
            _ => return,
        }
    }
}

/// Highlights the Starlark code `text`, which starts at byte `offset`.
fn starlark(text: &str, offset: usize, c: &mut Collector) {
    for t in lex(text) {
        let class = match t.kind {
            TokenKind::Keyword if is_reserved(t.text(text)) => Class::Reserved,
            TokenKind::Keyword => Class::Keyword,
            TokenKind::Ident => match t.text(text) {
                "True" | "False" | "None" => Class::Constant,
                _ => continue,
            },
            TokenKind::Int | TokenKind::Float => Class::Number,
            TokenKind::Str(_) => Class::String,
            TokenKind::Comment => Class::Comment,
            TokenKind::Error => Class::Error,
            TokenKind::Open(bracket) => {
                c.bracket(offset + t.start, bracket, true);
                continue;
            }
            TokenKind::Close(bracket) => {
                c.bracket(offset + t.start, bracket, false);
                continue;
            }
            TokenKind::Comma
            | TokenKind::Dot
            | TokenKind::Colon
            | TokenKind::Semicolon
            | TokenKind::Assign
            | TokenKind::Arrow
            | TokenKind::Op
            | TokenKind::Newline
            | TokenKind::Continuation => continue,
        };
        c.span(offset + t.start, offset + t.end, class);
    }
}

/// Highlights the query `text`, which starts at byte `offset`, the way the query grammar splits
/// it: quoted words are strings, words of digits numbers, a word followed by `(` a function,
/// and `union`, `except` and `intersect` operators.
fn query(text: &str, offset: usize, c: &mut Collector) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'"' | b'\'' => {
                // To the closing quote, or the end of the query.
                let end = bytes
                    .get(i + 1..)
                    .and_then(|rest| rest.iter().position(|&q| q == b))
                    .map_or(bytes.len(), |j| i + 1 + j + 1);
                c.span(offset + i, offset + end, Class::String);
                i = end;
            }
            b'(' | b')' => {
                c.bracket(offset + i, Bracket::Paren, b == b'(');
                i += 1;
            }
            _ if is_word_char(char::from(b)) => {
                let len = bytes
                    .get(i..)
                    .unwrap_or_default()
                    .iter()
                    .take_while(|&&b| is_word_char(char::from(b)))
                    .count();
                let end = i + len;
                let word = text.get(i..end).unwrap_or("");
                let next = bytes
                    .get(end..)
                    .unwrap_or_default()
                    .iter()
                    .find(|b| !b.is_ascii_whitespace());
                let class = if OPERATOR_WORDS.contains(&word) {
                    Some(Class::Keyword)
                } else if word.bytes().all(|b| b.is_ascii_digit()) {
                    Some(Class::Number)
                } else if next == Some(&b'(') {
                    Some(Class::Function)
                } else {
                    None
                };
                if let Some(class) = class {
                    c.span(offset + i, offset + end, class);
                }
                i = end;
            }
            // Spaces, operators, commas, and the bytes of other characters (none of which is
            // ASCII, so no span starts or ends inside a character).
            _ => i += 1,
        }
    }
}

/// The position of the bracket that matches the bracket at `cursor`, or else the one just
/// before it (so that the bracket just typed shows its match). `None` if neither is a bracket,
/// or it has no match (unbalanced, or closed by a bracket of another kind).
fn matching_bracket(brackets: &[BracketAt], cursor: usize) -> Option<usize> {
    let index = brackets.iter().position(|b| b.pos == cursor).or_else(|| {
        let before = cursor.checked_sub(1)?;
        brackets.iter().position(|b| b.pos == before)
    })?;
    let at = brackets.get(index)?;
    let mut depth = 0usize;
    let partner = if at.open {
        brackets.get(index + 1..)?.iter().find(|b| {
            if b.open {
                depth += 1;
                false
            } else if depth == 0 {
                true
            } else {
                depth -= 1;
                false
            }
        })
    } else {
        brackets.get(..index)?.iter().rev().find(|b| {
            if !b.open {
                depth += 1;
                false
            } else if depth == 0 {
                true
            } else {
                depth -= 1;
                false
            }
        })
    }?;
    (partner.bracket == at.bracket).then_some(partner.pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The highlighted parts of `buf`, as `(text, class)`.
    fn parts(buf: &str, cursor: Option<usize>) -> Vec<(&str, Class)> {
        spans(buf, cursor)
            .into_iter()
            .map(|s| (&buf[s.start..s.end], s.class))
            .collect()
    }

    /// Removes the SGR sequences from `s`.
    fn strip(s: &str) -> String {
        let mut out = String::new();
        let mut rest = s;
        while let Some(i) = rest.find("\x1b[") {
            out.push_str(&rest[..i]);
            let after = &rest[i + 2..];
            let end = after.find('m').expect("an SGR sequence ends with m");
            assert!(
                after[..end]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b == b';'),
                "not an SGR sequence: {:?}",
                &after[..end]
            );
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn test_starlark() {
        assert_eq!(
            parts("def f(x = None):  # hi\n    return \"a\" + 1.5", None),
            vec![
                ("def", Class::Keyword),
                ("None", Class::Constant),
                ("# hi", Class::Comment),
                ("return", Class::Keyword),
                ("\"a\"", Class::String),
                ("1.5", Class::Number),
            ]
        );
        assert_eq!(
            parts("while x and True: 0x1F", None),
            vec![
                ("while", Class::Reserved),
                ("and", Class::Keyword),
                ("True", Class::Constant),
                ("0x1F", Class::Number),
            ]
        );
        // Identifiers, operators and punctuation are not highlighted.
        assert_eq!(
            parts("ctx.cquery().deps(x, y=z)[0:1]", None),
            vec![("0", Class::Number), ("1", Class::Number)]
        );
        assert_eq!(parts("a $ b", None), vec![("$", Class::Error)]);
        // An unterminated string, and prefixed and triple-quoted ones.
        assert_eq!(
            parts("x = b'a' + \"\"\"b\nc\"\"\" + 'unterminated", None),
            vec![
                ("b'a'", Class::String),
                ("\"\"\"b\nc\"\"\"", Class::String),
                ("'unterminated", Class::String),
            ]
        );
        assert_eq!(parts("", Some(0)), vec![]);
        assert_eq!(parts("   ", Some(1)), vec![]);
    }

    #[test]
    fn test_commands() {
        assert_eq!(
            parts(":build //x:y", None),
            vec![(":build", Class::Command)]
        );
        assert_eq!(parts("  :b //x:y", None), vec![(":b", Class::Command)]);
        assert_eq!(parts(":zz 1", None), vec![(":zz", Class::UnknownCommand)]);
        // Being typed: the colon alone, a prefix of several commands.
        assert_eq!(parts(":", None), vec![(":", Class::Command)]);
        assert_eq!(parts(":re", None), vec![(":re", Class::Command)]);
        assert_eq!(parts(":!ls 'a'", None), vec![(":!", Class::Command)]);
        // Starlark arguments are highlighted, other arguments are not.
        assert_eq!(
            parts(":p [1, 'a']", None),
            vec![
                (":p", Class::Command),
                ("1", Class::Number),
                ("'a'", Class::String)
            ]
        );
        assert_eq!(
            parts(":load x.bzl 'a'", None),
            vec![(":load", Class::Command)]
        );
        assert_eq!(
            parts(":time :t None", None),
            vec![
                (":time", Class::Command),
                (":t", Class::Command),
                ("None", Class::Constant),
            ]
        );
        assert_eq!(
            parts(":time for i in x: pass", None),
            vec![
                (":time", Class::Command),
                ("for", Class::Keyword),
                ("in", Class::Keyword),
                ("pass", Class::Keyword),
            ]
        );
        // An unknown command's argument is left alone.
        assert_eq!(
            parts(":zz None", None),
            vec![(":zz", Class::UnknownCommand)]
        );
    }

    #[test]
    fn test_queries() {
        assert_eq!(
            parts(":cq deps(//x:y, 1) union kind('rule', //...)", None),
            vec![
                (":cq", Class::Command),
                ("deps", Class::Function),
                ("1", Class::Number),
                ("union", Class::Keyword),
                ("kind", Class::Function),
                ("'rule'", Class::String),
            ]
        );
        assert_eq!(
            parts(":uq attrfilter (\"a\", 'unterminated", None),
            vec![
                (":uq", Class::Command),
                ("attrfilter", Class::Function),
                ("\"a\"", Class::String),
                ("'unterminated", Class::String),
            ]
        );
        // Brackets in a query match too.
        assert_eq!(
            parts(":cq deps(set(a b))", Some(17)),
            vec![
                (":cq", Class::Command),
                ("deps", Class::Function),
                ("(", Class::MatchingBracket),
                ("set", Class::Function),
            ]
        );
        // Non-ASCII text is skipped whole.
        assert_eq!(
            parts(":cq é(x) 'ü'", None),
            vec![(":cq", Class::Command), ("'ü'", Class::String)]
        );
    }

    #[test]
    fn test_matching_bracket() {
        let buf = "f(a[1], {2: (3)})";
        let matching = |cursor| {
            spans(buf, Some(cursor))
                .into_iter()
                .find(|s| s.class == Class::MatchingBracket)
                .map(|s| s.start)
        };
        // Just after a closing bracket (just typed), and on it.
        assert_eq!(matching(buf.len()), Some(1));
        assert_eq!(matching(buf.len() - 1), Some(1));
        // On an opening bracket.
        assert_eq!(matching(1), Some(16));
        assert_eq!(matching(3), Some(5));
        assert_eq!(matching(9), Some(15));
        assert_eq!(matching(12), Some(14));
        // The bracket under the cursor wins over the one before it.
        assert_eq!(matching(15), Some(8));
        assert_eq!(matching(14), Some(12));
        // Not at a bracket.
        assert_eq!(matching(0), None);
        assert_eq!(matching(11), None);
        // Brackets in strings and comments do not count.
        let buf = "f(')', # )\n x)";
        let m = spans(buf, Some(buf.len()));
        assert_eq!(
            m.iter()
                .find(|s| s.class == Class::MatchingBracket)
                .map(|s| s.start),
            Some(1)
        );
        // Unbalanced or mismatched: nothing.
        assert!(
            !spans("(]", Some(2))
                .iter()
                .any(|s| s.class == Class::MatchingBracket)
        );
        assert!(
            !spans("x)", Some(2))
                .iter()
                .any(|s| s.class == Class::MatchingBracket)
        );
        assert!(
            !spans("(x", Some(1))
                .iter()
                .any(|s| s.class == Class::MatchingBracket)
        );
        // In the argument of a command, at its offset.
        assert_eq!(
            parts(":p f(x)", Some(7)),
            vec![(":p", Class::Command), ("(", Class::MatchingBracket)]
        );
    }

    #[test]
    fn test_paint() {
        let buf = "x = \"a\" # c";
        let painted = highlight(buf, None).unwrap();
        assert_eq!(painted, "x = \x1b[32m\"a\"\x1b[0m \x1b[2m# c\x1b[0m");
        // A span over several lines is painted line by line.
        let buf = "s = \"\"\"a\n\nb\"\"\"\nx";
        let painted = highlight(buf, None).unwrap();
        assert_eq!(
            painted,
            "s = \x1b[32m\"\"\"a\x1b[0m\n\n\x1b[32mb\"\"\"\x1b[0m\nx"
        );
        assert_eq!(highlight("x = y", Some(5)), None);
        // Bad spans are skipped.
        let bad = [
            Span {
                start: 2,
                end: 1,
                class: Class::Number,
            },
            Span {
                start: 0,
                end: 99,
                class: Class::Number,
            },
            Span {
                start: 1,
                end: 2,
                class: Class::Number,
            },
            Span {
                start: 0,
                end: 1,
                class: Class::Number,
            },
        ];
        assert_eq!(strip(&paint("éa", &bad, ansi_style)), "éa");
    }

    #[test]
    fn test_paint_keeps_the_text() {
        let inputs = [
            "def f(x):\n    return [x, 'a', 1, None]  # c",
            ":time :p {'a': (1, 2.5e3)}",
            ":cq deps('//x:y') except set(a b)",
            "\"\"\"never closed\n\n",
            "é = 'ü' + \"\\\"\" $ ?",
            ":zz",
            "f(\"a\nb)",
            "x = f\"{y}\" + rb'z'\t# tab",
        ];
        for buf in inputs {
            for cursor in 0..=buf.len() {
                if !buf.is_char_boundary(cursor) {
                    continue;
                }
                let spans = spans(buf, Some(cursor));
                for pair in spans.windows(2) {
                    assert!(pair[0].end <= pair[1].start, "{buf:?}: {spans:?}");
                }
                for s in &spans {
                    assert!(buf.get(s.start..s.end).is_some(), "{buf:?}: {s:?}");
                }
                let painted = paint(buf, &spans, ansi_style);
                assert_eq!(strip(&painted), buf);
                // Every line ends with its colour reset.
                for line in painted.split('\n') {
                    let after_reset = line.rsplit(RESET).next().unwrap_or("");
                    assert!(!after_reset.contains("\x1b["), "{line:?}");
                }
            }
        }
    }
}
