/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Whether an input buffer is ready to be submitted, or needs more lines.
//!
//! This is the line editor's validator: Enter submits a complete buffer and inserts a newline
//! into an incomplete one.

use crate::lexer::BLOCK_KEYWORDS;
use crate::lexer::LexState;
use crate::lexer::Token;
use crate::lexer::TokenKind;
use crate::lexer::lex_from;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Completeness {
    Complete,
    Incomplete(Incomplete),
}

/// Why a buffer is incomplete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Incomplete {
    /// A bracket is not closed.
    Bracket,
    /// A triple-quoted string is not terminated.
    TripleQuote,
    /// The buffer ends with a line-continuing backslash.
    Continuation,
    /// The last line ends with `:`, so a block must follow.
    Colon,
    /// A block is open: more lines may follow until a blank line.
    Block,
}

/// Whether `buf` is ready to be submitted.
///
/// A meta-command (first non-space character `:`) is complete unless it ends with `\`.
/// Starlark is incomplete when:
/// - a bracket is not closed;
/// - a triple-quoted string is not terminated;
/// - it ends with a line-continuing `\`;
/// - the last significant token of the last line is `:`;
/// - it has more than one line, its last line is not blank, and either its first logical line
///   is a block header (`def`, `if`, `for`, ...) or a later logical line is indented more than
///   the first one. This is Python's rule: a blank line ends a block.
///
/// Everything else is complete, including an unterminated single-quoted string: the parser
/// reports it.
pub fn completeness(buf: &str) -> Completeness {
    if buf.trim_start().starts_with(':') {
        return if buf.ends_with('\\') {
            Completeness::Incomplete(Incomplete::Continuation)
        } else {
            Completeness::Complete
        };
    }

    let (tokens, end) = lex_from(buf, LexState::default());
    if let Some(open) = end.open_string {
        return Completeness::Incomplete(if open.quote.is_triple() {
            Incomplete::TripleQuote
        } else {
            Incomplete::Continuation
        });
    }
    if matches!(
        tokens.last(),
        Some(Token {
            kind: TokenKind::Continuation,
            ..
        })
    ) {
        return Completeness::Incomplete(Incomplete::Continuation);
    }
    if tokens.iter().any(is_unterminated_short_string) {
        // More lines cannot fix it: let the parser report it.
        return Completeness::Complete;
    }

    let lines = LogicalLines::scan(buf, &tokens);
    if lines.depth > 0 {
        return Completeness::Incomplete(Incomplete::Bracket);
    }

    let last_line_start = buf.rfind('\n').map_or(0, |i| i + 1);
    let last_significant = tokens.iter().rev().find(|t| t.kind != TokenKind::Comment);
    if let Some(t) = last_significant {
        if t.kind == TokenKind::Colon && t.start >= last_line_start {
            return Completeness::Incomplete(Incomplete::Colon);
        }
    }

    let last_line = buf.get(last_line_start..).unwrap_or("");
    if last_line_start > 0
        && !last_line.trim().is_empty()
        && (lines.first_is_header || lines.later_indented)
    {
        return Completeness::Incomplete(Incomplete::Block);
    }
    Completeness::Complete
}

/// A single-quoted string that ends at a newline or at the end of the text.
pub(crate) fn is_unterminated_short_string(t: &Token) -> bool {
    matches!(t.kind, TokenKind::Str(info) if !info.closed && !info.quote.is_triple())
}

/// [`completeness`] is [`Completeness::Complete`].
pub fn is_complete(buf: &str) -> bool {
    completeness(buf) == Completeness::Complete
}

/// Whether the first word of `line` (after indentation) is a block keyword.
pub fn is_block_header(line: &str) -> bool {
    BLOCK_KEYWORDS.contains(&crate::lexer::leading_word(line.trim_start()))
}

/// What [`completeness`] needs to know about the logical lines of a buffer.
struct LogicalLines {
    /// Bracket depth at the end.
    depth: usize,
    /// The first logical line starts with a block keyword.
    first_is_header: bool,
    /// A later logical line is indented more than the first one.
    later_indented: bool,
}

impl LogicalLines {
    fn scan(buf: &str, tokens: &[Token]) -> Self {
        let mut depth = 0usize;
        let mut at_line_start = true;
        let mut first: Option<usize> = None;
        let mut first_is_header = false;
        let mut later_indented = false;
        let mut previous: Option<TokenKind> = None;
        for t in tokens {
            match t.kind {
                TokenKind::Newline => {
                    // A newline inside brackets or after a continuation does not end the line.
                    if depth == 0 && previous != Some(TokenKind::Continuation) {
                        at_line_start = true;
                    }
                }
                // Comment-only lines do not start logical lines.
                TokenKind::Comment => {}
                kind => {
                    if at_line_start {
                        at_line_start = false;
                        let indent = indentation_at(buf, t.start);
                        match first {
                            None => {
                                first = Some(indent);
                                first_is_header = t.kind == TokenKind::Keyword
                                    && BLOCK_KEYWORDS.contains(&t.text(buf));
                            }
                            Some(base) => later_indented |= indent > base,
                        }
                    }
                    match kind {
                        TokenKind::Open(_) => depth += 1,
                        TokenKind::Close(_) => depth = depth.saturating_sub(1),
                        _ => {}
                    }
                }
            }
            previous = Some(t.kind);
        }
        LogicalLines {
            depth,
            first_is_header,
            later_indented,
        }
    }
}

/// Number of bytes between the start of the line containing `pos` and `pos`.
fn indentation_at(buf: &str, pos: usize) -> usize {
    let before = buf.get(..pos).unwrap_or("");
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    pos.saturating_sub(line_start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(buf: &str) -> Completeness {
        completeness(buf)
    }

    const COMPLETE: Completeness = Completeness::Complete;

    fn incomplete(why: Incomplete) -> Completeness {
        Completeness::Incomplete(why)
    }

    #[test]
    fn test_simple() {
        assert_eq!(check(""), COMPLETE);
        assert_eq!(check("x"), COMPLETE);
        assert_eq!(check("x = 1"), COMPLETE);
        assert_eq!(check("  x = 1"), COMPLETE);
        assert_eq!(check("x = 1  # comment"), COMPLETE);
        assert_eq!(check("x = 1\ny = 2"), COMPLETE);
        assert_eq!(check("if x: pass"), COMPLETE);
        assert!(is_complete("f(1)"));
    }

    #[test]
    fn test_commands() {
        assert_eq!(check(":b //foo"), COMPLETE);
        assert_eq!(check("  :help"), COMPLETE);
        assert_eq!(check(":cq deps("), COMPLETE);
        assert_eq!(check(":t ctx.cquery(\n"), COMPLETE);
        assert_eq!(check(":b //foo \\"), incomplete(Incomplete::Continuation));
        assert_eq!(check(":b //foo \\\n//bar"), COMPLETE);
    }

    #[test]
    fn test_brackets() {
        assert_eq!(check("f("), incomplete(Incomplete::Bracket));
        assert_eq!(check("x = [1,\n  2,"), incomplete(Incomplete::Bracket));
        assert_eq!(check("x = {'a': [1, 2"), incomplete(Incomplete::Bracket));
        assert_eq!(check("x = [\n  1,\n]"), COMPLETE);
        assert_eq!(check("f(\n  a,\n  b)"), COMPLETE);
        // Brackets in strings and comments do not count.
        assert_eq!(check("x = '(' # ["), COMPLETE);
        // Extra closing brackets are the parser's problem.
        assert_eq!(check("x = 1)"), COMPLETE);
    }

    #[test]
    fn test_strings() {
        assert_eq!(check("x = '''abc"), incomplete(Incomplete::TripleQuote));
        assert_eq!(
            check("x = \"\"\"a\n\nb"),
            incomplete(Incomplete::TripleQuote)
        );
        assert_eq!(check("x = '''a\nb'''"), COMPLETE);
        // An unterminated single-quoted string is reported by the parser, even inside brackets.
        assert_eq!(check("x = 'abc"), COMPLETE);
        assert_eq!(check("print(\"hello"), COMPLETE);
        assert_eq!(check("f(\n  'abc\n"), COMPLETE);
        // ... unless its last character escapes the newline.
        assert_eq!(check("x = 'abc\\"), incomplete(Incomplete::Continuation));
    }

    #[test]
    fn test_continuation() {
        assert_eq!(check("x = 1 + \\"), incomplete(Incomplete::Continuation));
        assert_eq!(check("x = 1 + \\\n  2"), COMPLETE);
        // A backslash in a comment does not continue the line.
        assert_eq!(check("x = 1  # \\"), COMPLETE);
    }

    #[test]
    fn test_colon() {
        assert_eq!(check("def f():"), incomplete(Incomplete::Colon));
        assert_eq!(check("def f():  # comment"), incomplete(Incomplete::Colon));
        assert_eq!(check("for x in y:"), incomplete(Incomplete::Colon));
        assert_eq!(
            check("if x:\n    pass\nelse:"),
            incomplete(Incomplete::Colon)
        );
        assert_eq!(check("f = lambda x:"), incomplete(Incomplete::Colon));
        assert_eq!(check("def f(\n  a,\n):"), incomplete(Incomplete::Colon));
        // A colon inside brackets is a bracket problem first.
        assert_eq!(check("{'a':"), incomplete(Incomplete::Bracket));
    }

    #[test]
    fn test_blocks() {
        assert_eq!(
            check("def f():\n    return 1"),
            incomplete(Incomplete::Block)
        );
        assert_eq!(check("def f():\n    return 1\n"), COMPLETE);
        assert_eq!(check("def f():\n    return 1\n   "), COMPLETE);
        assert_eq!(
            check("if x:\n    y = 1\nelse:\n    y = 2"),
            incomplete(Incomplete::Block)
        );
        assert_eq!(check("if x:\n    y = 1\nelse:\n    y = 2\n"), COMPLETE);
        assert_eq!(
            check("for x in y:\n  print(x)"),
            incomplete(Incomplete::Block)
        );
        // A later line that is indented keeps the block open too.
        assert_eq!(check("x = 1\n  y = 2"), incomplete(Incomplete::Block));
        // ... relative to the first line.
        assert_eq!(check("  x = 1\n  y = 2"), COMPLETE);
        // A comment line is not blank, but does not start a logical line.
        assert_eq!(
            check("def f():\n  pass\n# done"),
            incomplete(Incomplete::Block)
        );
        assert_eq!(check("x = 1\n    # indented comment"), COMPLETE);
        // Lines inside brackets are not logical lines.
        assert_eq!(check("x = f(\n    1,\n    2,\n)"), COMPLETE);
        // A header whose body is on the same line still waits for `else`.
        assert_eq!(
            check("if x: y = 1\nelse: y = 2"),
            incomplete(Incomplete::Block)
        );
        // Lines inside a triple-quoted string are not logical lines.
        assert_eq!(check("x = '''\n    a\n'''"), COMPLETE);
    }

    #[test]
    fn test_is_block_header() {
        assert!(is_block_header("def f():"));
        assert!(is_block_header("  if(x):"));
        assert!(is_block_header("else:"));
        assert!(!is_block_header("iffy = 1"));
        assert!(!is_block_header("for_x = 1"));
        assert!(!is_block_header(":def"));
    }
}
