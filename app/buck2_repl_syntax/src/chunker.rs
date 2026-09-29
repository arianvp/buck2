/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Splits piped input into REPL inputs ("chunks"), line by line.
//!
//! Each chunk is emitted as soon as it is known to be complete, so that a program driving the
//! REPL through a pipe sees each result before it sends the next input:
//! - a simple statement (not a block header) is emitted as soon as it is complete;
//! - a compound statement (`def`, `if`, `for`, ...) is emitted at a blank line, at a line that
//!   is not indented more than its first line (except `elif`/`else`), or at the end of input;
//! - a line whose first non-space character is `:` (a meta-command) is its own chunk, joined
//!   with the following lines while it ends with `\`;
//! - blank and comment-only lines between chunks are skipped.

use crate::completeness::is_block_header;
use crate::completeness::is_unterminated_short_string;
use crate::lexer::LexState;
use crate::lexer::TokenKind;
use crate::lexer::leading_word;
use crate::lexer::lex_from;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// No chunk in progress.
    Idle,
    /// A meta-command continued with `\`.
    Command,
    /// A simple statement.
    Simple,
    /// A compound statement.
    Compound,
}

/// See the module documentation.
#[derive(Debug, Clone)]
pub struct Chunker {
    buf: String,
    kind: Kind,
    /// Indentation of the chunk's first line.
    base_indent: usize,
    /// Lexer state at the end of `buf`.
    state: LexState,
    /// Bracket depth at the end of `buf`.
    depth: usize,
    /// The last line ends with a line-continuing `\`.
    continuation: bool,
    /// The last significant token of the last line is `:`.
    trailing_colon: bool,
}

impl Default for Chunker {
    fn default() -> Self {
        Self::new()
    }
}

impl Chunker {
    pub fn new() -> Self {
        Chunker {
            buf: String::new(),
            kind: Kind::Idle,
            base_indent: 0,
            state: LexState::default(),
            depth: 0,
            continuation: false,
            trailing_colon: false,
        }
    }

    /// Adds one line of input (without its line terminator; a trailing `\r` is dropped) and
    /// returns the chunks it completes, in order.
    pub fn push_line(&mut self, line: &str) -> Vec<String> {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let mut out = Vec::new();
        self.push(line, &mut out);
        out
    }

    /// Ends the input: returns the chunk in progress, if any.
    pub fn finish(&mut self) -> Option<String> {
        let chunk = self.take();
        if chunk.trim().is_empty() {
            None
        } else {
            Some(chunk)
        }
    }

    /// No chunk is in progress.
    pub fn is_idle(&self) -> bool {
        self.kind == Kind::Idle
    }

    fn push(&mut self, line: &str, out: &mut Vec<String>) {
        match self.kind {
            Kind::Idle => self.start(line, out),
            Kind::Command => {
                self.buf.push('\n');
                self.buf.push_str(line);
                if !line.ends_with('\\') {
                    out.push(self.take());
                }
            }
            Kind::Simple | Kind::Compound => {
                let at_boundary =
                    self.state.open_string.is_none() && self.depth == 0 && !self.continuation;
                if at_boundary {
                    let trimmed = line.trim_start();
                    if trimmed.is_empty() {
                        // A blank line ends the chunk.
                        out.push(self.take());
                        return;
                    }
                    let indent = line.len() - trimmed.len();
                    let continues_block = self.kind == Kind::Compound
                        && matches!(leading_word(trimmed), "elif" | "else");
                    if indent <= self.base_indent && !trimmed.starts_with('#') && !continues_block {
                        out.push(self.take());
                        self.start(line, out);
                        return;
                    }
                }
                self.append_code(line);
                self.emit_if_complete(out);
            }
        }
    }

    /// Starts a chunk with `line`, from the idle state.
    fn start(&mut self, line: &str, out: &mut Vec<String>) {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return;
        }
        if trimmed.starts_with(':') {
            if line.ends_with('\\') {
                self.kind = Kind::Command;
                self.buf.push_str(line);
            } else {
                out.push(line.to_owned());
            }
            return;
        }
        self.base_indent = line.len() - trimmed.len();
        self.kind = if is_block_header(trimmed) {
            Kind::Compound
        } else {
            Kind::Simple
        };
        self.append_code(line);
        self.emit_if_complete(out);
    }

    fn append_code(&mut self, line: &str) {
        if !self.buf.is_empty() {
            self.buf.push('\n');
        }
        self.buf.push_str(line);
        let (tokens, state) = lex_from(line, self.state);
        self.state = state;
        for t in &tokens {
            match t.kind {
                TokenKind::Open(_) => self.depth += 1,
                TokenKind::Close(_) => self.depth = self.depth.saturating_sub(1),
                _ => {}
            }
        }
        if tokens.iter().any(is_unterminated_short_string) {
            // The chunk cannot parse whatever follows: stop waiting for its brackets.
            self.depth = 0;
        }
        self.continuation = matches!(tokens.last(), Some(t) if t.kind == TokenKind::Continuation);
        self.trailing_colon = matches!(
            tokens.iter().rev().find(|t| t.kind != TokenKind::Comment),
            Some(t) if t.kind == TokenKind::Colon
        );
    }

    fn emit_if_complete(&mut self, out: &mut Vec<String>) {
        if self.kind == Kind::Simple
            && self.state.open_string.is_none()
            && self.depth == 0
            && !self.continuation
            && !self.trailing_colon
        {
            out.push(self.take());
        }
    }

    /// Returns the chunk in progress and goes back to the idle state.
    fn take(&mut self) -> String {
        std::mem::take(self).buf
    }
}

/// Splits a whole script into chunks.
pub fn split_script(script: &str) -> Vec<String> {
    let mut chunker = Chunker::new();
    let mut out = Vec::new();
    for line in script.lines() {
        out.extend(chunker.push_line(line));
    }
    out.extend(chunker.finish());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(script: &str) -> Vec<String> {
        split_script(script)
    }

    /// Feeds `script` line by line, and records after which line each chunk came out.
    fn emitted_after(script: &str) -> Vec<(usize, String)> {
        let mut chunker = Chunker::new();
        let mut out = Vec::new();
        for (i, line) in script.lines().enumerate() {
            for c in chunker.push_line(line) {
                out.push((i + 1, c));
            }
        }
        if let Some(c) = chunker.finish() {
            out.push((usize::MAX, c));
        }
        out
    }

    #[test]
    fn test_simple_statements_are_emitted_at_once() {
        assert_eq!(
            emitted_after("x = 1\nx + 1\nprint(x)"),
            vec![
                (1, "x = 1".to_owned()),
                (2, "x + 1".to_owned()),
                (3, "print(x)".to_owned()),
            ]
        );
    }

    #[test]
    fn test_def_then_column_zero_call() {
        assert_eq!(
            emitted_after("def f(n):\n    return n * 3\nf(4)"),
            vec![
                (3, "def f(n):\n    return n * 3".to_owned()),
                (3, "f(4)".to_owned()),
            ]
        );
    }

    #[test]
    fn test_if_else() {
        assert_eq!(
            chunks("if x:\n    y = 1\nelif z:\n    y = 2\nelse:\n    y = 3\ny"),
            vec![
                "if x:\n    y = 1\nelif z:\n    y = 2\nelse:\n    y = 3",
                "y"
            ]
        );
    }

    #[test]
    fn test_blank_lines() {
        // A blank line ends a compound statement.
        assert_eq!(
            emitted_after("for i in range(3):\n    print(i)\n\n\nx"),
            vec![
                (3, "for i in range(3):\n    print(i)".to_owned()),
                (5, "x".to_owned()),
            ]
        );
        // Blank lines and comments between chunks are skipped.
        assert_eq!(
            chunks("\n\n# setup\nx = 1\n   \n  # more\n\ny = 2\n"),
            vec!["x = 1", "y = 2"]
        );
        // A blank line inside brackets or a triple-quoted string does not end anything.
        assert_eq!(
            chunks("x = [\n\n  1,\n]\ns = '''a\n\nb'''\ns"),
            vec!["x = [\n\n  1,\n]", "s = '''a\n\nb'''", "s"]
        );
    }

    #[test]
    fn test_commands() {
        assert_eq!(
            emitted_after(":t 1\nx = 1\n  :b //foo\n:cq deps(\n"),
            vec![
                (1, ":t 1".to_owned()),
                (2, "x = 1".to_owned()),
                (3, "  :b //foo".to_owned()),
                (4, ":cq deps(".to_owned()),
            ]
        );
        // A column-0 command ends a compound statement.
        assert_eq!(
            chunks("def f():\n    return 1\n:t f()"),
            vec!["def f():\n    return 1", ":t f()"]
        );
        // A command continued with a backslash.
        assert_eq!(chunks(":b //a \\\n  //b\nx"), vec![":b //a \\\n  //b", "x"]);
        // A command-looking line inside a string is not a command.
        assert_eq!(chunks("x = '''\n:b\n'''"), vec!["x = '''\n:b\n'''"]);
    }

    #[test]
    fn test_multi_line_simple_statements() {
        assert_eq!(
            emitted_after("x = f(\n    1,\n    2,\n)\ny"),
            vec![
                (4, "x = f(\n    1,\n    2,\n)".to_owned()),
                (5, "y".to_owned()),
            ]
        );
        assert_eq!(
            emitted_after("x = 1 + \\\n    2\ny"),
            vec![(2, "x = 1 + \\\n    2".to_owned()), (3, "y".to_owned())]
        );
        assert_eq!(
            emitted_after("s = \"\"\"\nabc\n\"\"\""),
            vec![(3, "s = \"\"\"\nabc\n\"\"\"".to_owned())]
        );
    }

    #[test]
    fn test_compound_details() {
        // A header split over lines.
        assert_eq!(
            chunks("def f(\n    a,\n):\n    return a\nf(1)"),
            vec!["def f(\n    a,\n):\n    return a", "f(1)"]
        );
        // A comment at column 0 inside a block does not end it.
        assert_eq!(
            chunks("def f():\n    x = 1\n# note\n    return x\nf()"),
            vec!["def f():\n    x = 1\n# note\n    return x", "f()"]
        );
        // An indented script is chunked relative to its own indentation.
        assert_eq!(
            chunks("  def f():\n      return 1\n  f()"),
            vec!["  def f():\n      return 1", "  f()"]
        );
        // Nested blocks.
        assert_eq!(
            chunks("for x in y:\n  if x:\n    print(x)\n  else:\n    pass\nz"),
            vec!["for x in y:\n  if x:\n    print(x)\n  else:\n    pass", "z"]
        );
        // `else` right after a simple statement is a new (broken) chunk.
        assert_eq!(chunks("x = 1\nelse:\n  y"), vec!["x = 1", "else:\n  y"]);
    }

    #[test]
    fn test_end_of_input() {
        assert_eq!(chunks("def f():\n  return 1"), vec!["def f():\n  return 1"]);
        assert_eq!(chunks("x = [1,"), vec!["x = [1,"]);
        assert_eq!(chunks(":b //a \\"), vec![":b //a \\"]);
        assert_eq!(chunks(""), Vec::<String>::new());
        let mut chunker = Chunker::new();
        assert!(chunker.push_line("x = (").is_empty());
        assert!(!chunker.is_idle());
        assert_eq!(chunker.finish().as_deref(), Some("x = ("));
        assert!(chunker.is_idle());
        assert_eq!(chunker.finish(), None);
    }

    #[test]
    fn test_broken_input_does_not_swallow_the_rest() {
        // An unterminated string inside brackets: emitted at once for the parser to report.
        assert_eq!(
            emitted_after("print(\"hello\nx"),
            vec![(1, "print(\"hello".to_owned()), (2, "x".to_owned())]
        );
        // A trailing colon on a simple statement waits for one more line.
        assert_eq!(
            chunks("f = lambda x:\n  x\ny"),
            vec!["f = lambda x:\n  x", "y"]
        );
    }

    #[test]
    fn test_crlf() {
        assert_eq!(chunks("x = 1\r\ny = 2\r\n"), vec!["x = 1", "y = 2"]);
    }
}
