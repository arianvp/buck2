/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Text helpers: dedent and prechecks for inputs, Starlark string literals, and capped output.

use std::borrow::Cow;
use std::fmt;

use crate::lexer::TokenKind;
use crate::lexer::lex;

/// Largest input the daemon evaluates.
pub const MAX_INPUT_BYTES: usize = 1 << 20;

/// Deepest bracket nesting the daemon accepts: the parser and compiler recurse on nesting.
pub const MAX_BRACKET_DEPTH: usize = 64;

/// Longest run of unary operators the daemon accepts (`- - - x`, `not not x`), for the same
/// reason.
pub const MAX_UNARY_RUN: usize = 64;

/// Removes the leading whitespace common to all non-blank lines, so that indented code (e.g.
/// pasted from a function body) can be evaluated.
///
/// Lines that start inside a string literal are string content and are left alone; they do not
/// count towards the common indentation either.
pub fn dedent(code: &str) -> Cow<'_, str> {
    let in_string = lines_starting_in_string(code);
    let common = code
        .split('\n')
        .zip(in_string.iter())
        .filter(|(line, in_string)| !**in_string && !line.trim().is_empty())
        .map(|(line, _)| leading_spaces(line))
        .min()
        .unwrap_or(0);
    if common == 0 {
        return Cow::Borrowed(code);
    }
    let mut out = String::with_capacity(code.len());
    for (i, (line, in_string)) in code.split('\n').zip(in_string.iter()).enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if *in_string {
            out.push_str(line);
        } else {
            let strip = leading_spaces(line).min(common);
            out.push_str(line.get(strip..).unwrap_or(line));
        }
    }
    Cow::Owned(out)
}

fn leading_spaces(line: &str) -> usize {
    line.bytes().take_while(|b| *b == b' ').count()
}

/// For each line of `code` (split on `\n`), whether it starts inside a string literal.
fn lines_starting_in_string(code: &str) -> Vec<bool> {
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(code.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let mut in_string = vec![false; line_starts.len()];
    for t in lex(code) {
        if let TokenKind::Str(_) = t.kind {
            // The lines that start strictly inside the token.
            let first = line_starts.partition_point(|start| *start <= t.start);
            for line in first..line_starts.len() {
                match (line_starts.get(line), in_string.get_mut(line)) {
                    (Some(start), Some(slot)) if *start < t.end => *slot = true,
                    _ => break,
                }
            }
        }
    }
    in_string
}

/// Why the daemon refuses to evaluate an input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrecheckError {
    TooLarge { size: usize },
    Tab { line: usize, column: usize },
    TooDeep,
    UnaryRun,
}

impl fmt::Display for PrecheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrecheckError::TooLarge { size } => write!(
                f,
                "input is too large ({size} bytes; the limit is {MAX_INPUT_BYTES} bytes)"
            ),
            PrecheckError::Tab { line, column } => write!(
                f,
                "tabs are not allowed in Starlark; use spaces (line {line}, column {column})"
            ),
            PrecheckError::TooDeep => write!(
                f,
                "brackets are nested too deeply (the limit is {MAX_BRACKET_DEPTH})"
            ),
            PrecheckError::UnaryRun => write!(
                f,
                "too many unary operators in a row (the limit is {MAX_UNARY_RUN})"
            ),
        }
    }
}

impl std::error::Error for PrecheckError {}

/// Checks that `code` is safe to hand to the parser: at most [`MAX_INPUT_BYTES`], no tabs,
/// brackets nested at most [`MAX_BRACKET_DEPTH`] deep and at most [`MAX_UNARY_RUN`] unary
/// operators in a row.
pub fn precheck(code: &str) -> Result<(), PrecheckError> {
    if code.len() > MAX_INPUT_BYTES {
        return Err(PrecheckError::TooLarge { size: code.len() });
    }
    if let Some(pos) = code.find('\t') {
        let before = code.get(..pos).unwrap_or("");
        let line = before.matches('\n').count() + 1;
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        let column = before.get(line_start..).map_or(0, |s| s.chars().count()) + 1;
        return Err(PrecheckError::Tab { line, column });
    }
    let mut depth = 0usize;
    let mut unary_run = 0usize;
    for t in lex(code) {
        match t.kind {
            TokenKind::Open(_) => {
                depth += 1;
                if depth > MAX_BRACKET_DEPTH {
                    return Err(PrecheckError::TooDeep);
                }
            }
            TokenKind::Close(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
        let unary = match t.kind {
            TokenKind::Op => matches!(t.text(code), "-" | "+" | "~"),
            TokenKind::Keyword => t.text(code) == "not",
            // These do not break a run.
            TokenKind::Newline | TokenKind::Comment | TokenKind::Continuation => continue,
            _ => false,
        };
        if unary {
            unary_run += 1;
            if unary_run > MAX_UNARY_RUN {
                return Err(PrecheckError::UnaryRun);
            }
        } else {
            unary_run = 0;
        }
    }
    Ok(())
}

/// A double-quoted Starlark string literal whose value is `s`.
pub fn starlark_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' || c == '\x7f' => {
                // Infallible: writing to a `String`.
                let _ignored = fmt::Write::write_fmt(&mut out, format_args!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Splits `text` after its first `max_lines` lines: returns that prefix (including the newline
/// that ends its last line) and the number of lines left out. A final newline does not start
/// another line.
pub fn truncate_lines(text: &str, max_lines: usize) -> (&str, usize) {
    let mut kept = 0;
    for (i, _) in text.match_indices('\n') {
        kept += 1;
        if kept == max_lines {
            let end = i + 1;
            let rest = text.get(end..).unwrap_or("");
            if rest.is_empty() {
                return (text, 0);
            }
            let omitted = rest.split_terminator('\n').count();
            return (text.get(..end).unwrap_or(text), omitted);
        }
    }
    if max_lines == 0 && !text.is_empty() {
        return ("", text.split_terminator('\n').count());
    }
    (text, 0)
}

/// The longest prefix of `s` that is at most `max_bytes` long and ends on a character boundary.
pub fn truncate_to_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or("")
}

/// A [`fmt::Write`] that keeps at most `cap` bytes, silently drops the rest and never fails, so
/// that formatting a value into it cannot panic or run out of memory.
#[derive(Debug, Clone, Default)]
pub struct CappedString {
    buf: String,
    cap: usize,
    truncated: bool,
}

impl CappedString {
    pub fn new(cap: usize) -> Self {
        CappedString {
            buf: String::new(),
            cap,
            truncated: false,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.buf
    }

    pub fn into_string(self) -> String {
        self.buf
    }

    /// Some output was dropped.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

impl fmt::Write for CappedString {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.truncated {
            return Ok(());
        }
        let room = self.cap.saturating_sub(self.buf.len());
        if s.len() <= room {
            self.buf.push_str(s);
        } else {
            self.buf.push_str(truncate_to_bytes(s, room));
            self.truncated = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;

    use super::*;

    #[test]
    fn test_dedent() {
        assert_eq!(dedent("x = 1"), "x = 1");
        assert!(matches!(dedent("x = 1\n  y"), Cow::Borrowed(_)));
        assert_eq!(dedent("    x = 1\n    y = 2"), "x = 1\ny = 2");
        assert_eq!(
            dedent("    def f():\n        return 1\n\n    f()"),
            "def f():\n    return 1\n\nf()"
        );
        // Blank lines do not count, and lose at most the common indentation.
        assert_eq!(dedent("  a\n\n      \n  b"), "a\n\n    \nb");
        // String content is left alone.
        assert_eq!(
            dedent("    s = '''\nx\n      y'''\n    s"),
            "s = '''\nx\n      y'''\ns"
        );
        assert_eq!(dedent(""), "");
        assert_eq!(dedent("\n\n"), "\n\n");
    }

    #[test]
    fn test_precheck_ok() {
        assert_eq!(precheck("x = [[1], {'a': (2,)}]"), Ok(()));
        assert_eq!(precheck("x = - - 1\ny = not not x"), Ok(()));
        let deep = format!("{}{}", "[".repeat(64), "]".repeat(64));
        assert_eq!(precheck(&deep), Ok(()));
        let unary = format!("x = {}1", "-".repeat(64));
        assert_eq!(precheck(&unary), Ok(()));
        // Binary operators between operands do not make a run.
        let binary = format!("x = 1{}", " - 1".repeat(200));
        assert_eq!(precheck(&binary), Ok(()));
    }

    #[test]
    fn test_precheck_tabs() {
        assert_eq!(
            precheck("def f():\n\treturn 1"),
            Err(PrecheckError::Tab { line: 2, column: 1 })
        );
        assert_eq!(
            precheck("x = 'é\t'"),
            Err(PrecheckError::Tab { line: 1, column: 7 })
        );
        assert!(
            PrecheckError::Tab { line: 1, column: 1 }
                .to_string()
                .starts_with("tabs are not allowed in Starlark; use spaces")
        );
    }

    #[test]
    fn test_precheck_depth() {
        let deep = format!("{}{}", "(".repeat(65), ")".repeat(65));
        assert_eq!(precheck(&deep), Err(PrecheckError::TooDeep));
        // Unclosed brackets count too.
        assert_eq!(precheck(&"[".repeat(1000)), Err(PrecheckError::TooDeep));
        // Brackets in strings and comments do not.
        let quoted = format!("x = '{}' # {}", "(".repeat(100), "[".repeat(100));
        assert_eq!(precheck(&quoted), Ok(()));
    }

    #[test]
    fn test_precheck_unary() {
        let unary = format!("x = {}1", "-".repeat(65));
        assert_eq!(precheck(&unary), Err(PrecheckError::UnaryRun));
        let mixed = format!("x = {}1", "not - + ~ ".repeat(17));
        assert_eq!(precheck(&mixed), Err(PrecheckError::UnaryRun));
    }

    #[test]
    fn test_precheck_size() {
        let big = "x".repeat(MAX_INPUT_BYTES + 1);
        assert_eq!(
            precheck(&big),
            Err(PrecheckError::TooLarge {
                size: MAX_INPUT_BYTES + 1
            })
        );
        assert_eq!(precheck(&"x".repeat(MAX_INPUT_BYTES)), Ok(()));
    }

    #[test]
    fn test_starlark_string_literal() {
        assert_eq!(starlark_string_literal("abc"), "\"abc\"");
        assert_eq!(starlark_string_literal(""), "\"\"");
        assert_eq!(
            starlark_string_literal("a\"b\\c\nd\te\rf"),
            r#""a\"b\\c\nd\te\rf""#
        );
        assert_eq!(starlark_string_literal("\u{1}\u{7f}é"), r#""\x01\x7fé""#);
        assert_eq!(
            starlark_string_literal("deps(//foo:bar, 1)"),
            "\"deps(//foo:bar, 1)\""
        );
        // The literal lexes as one closed string.
        let lit = starlark_string_literal("x\"\n'''\\");
        let tokens = lex(&lit);
        assert_eq!(tokens.len(), 1);
        assert!(matches!(tokens[0].kind, TokenKind::Str(info) if info.closed));
    }

    #[test]
    fn test_truncate_lines() {
        assert_eq!(truncate_lines("a\nb\nc", 2), ("a\nb\n", 1));
        assert_eq!(truncate_lines("a\nb\nc\n", 2), ("a\nb\n", 1));
        assert_eq!(truncate_lines("a\nb\n", 2), ("a\nb\n", 0));
        assert_eq!(truncate_lines("a\nb", 2), ("a\nb", 0));
        assert_eq!(truncate_lines("a\nb\nc\nd", 1), ("a\n", 3));
        assert_eq!(truncate_lines("abc", 5), ("abc", 0));
        assert_eq!(truncate_lines("", 5), ("", 0));
        assert_eq!(truncate_lines("a\nb", 0), ("", 2));
    }

    #[test]
    fn test_truncate_to_bytes() {
        assert_eq!(truncate_to_bytes("abc", 5), "abc");
        assert_eq!(truncate_to_bytes("abc", 2), "ab");
        assert_eq!(truncate_to_bytes("é", 1), "");
        assert_eq!(truncate_to_bytes("aé", 2), "a");
    }

    #[test]
    fn test_capped_string() {
        let mut s = CappedString::new(5);
        write!(s, "{}", "abc").unwrap();
        assert!(!s.truncated());
        write!(s, "{}{}", "de", "fgh").unwrap();
        assert_eq!(s.as_str(), "abcde");
        assert!(s.truncated());
        write!(s, "more").unwrap();
        assert_eq!(s.len(), 5);
        let mut s = CappedString::new(3);
        write!(s, "aéb").unwrap();
        assert_eq!(s.into_string(), "aé");
        let mut s = CappedString::new(0);
        write!(s, "{}", 1).unwrap();
        assert!(s.is_empty() && s.truncated());
    }
}
