/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! A bound on how deeply the Starlark parser and compiler recurse on an input, computed without
//! parsing it.
//!
//! The parser, the compiler and the code that drops a syntax tree all recurse on the depth of
//! the tree, and the daemon aborts when a thread overflows its stack. Brackets are not the only
//! source of depth: `1+1+...`, `x.a.a...`, `f()()...`, `a and b and ...`,
//! `1 if a else 1 if b else ...`, `lambda: lambda: ...`, nested blocks and `elif` chains all
//! build deep trees from short inputs. (In a debug build on a 4 MiB stack, 1600 terms of `+`,
//! 550 nested lambdas or 800 nested `def`s overflow it.)
//!
//! The bound:
//!
//! - Inside a bracket pair, and in a logical line outside brackets, the elements between
//!   commas (and between `;` in a line) are separate subtrees.
//! - An element is at most as deep as the number of its *nesting tokens* (every token but
//!   names, numbers, plain strings and separators: each operator, attribute access, call,
//!   subscript, conditional or lambda node of the tree owns at least one), plus the depth of
//!   its deepest bracket pair. `lambda` counts twice, since a lambda costs the compiler about
//!   twice as much stack as an operator.
//! - A bracket pair is one level deeper than its deepest element. So is an f-string, whose
//!   `{...}` parts are its elements. F-strings are scanned the way Starlark lexes them: their
//!   expressions may contain strings, even with the f-string's own quotes.
//! - The commas of a lambda's parameter list do not end the element: the lambda's body is
//!   nested in the lambda.
//! - A statement is nested in its blocks, and in the `if` of every `elif`/`else` before it in
//!   its chain: [`STMT_WEIGHT`] per level.
//!
//! [`check`] rejects inputs where the bound for some logical line exceeds
//! [`MAX_NESTING`]. It also enforces the bracket depth and
//! unary-run limits of [`precheck`](crate::text::precheck), which it serves.

use crate::lexer::Bracket;
use crate::lexer::Quote;
use crate::lexer::Token;
use crate::lexer::TokenKind;
use crate::lexer::next_token;
use crate::text::MAX_BRACKET_DEPTH;
use crate::text::MAX_NESTING;
use crate::text::MAX_UNARY_RUN;
use crate::text::PrecheckError;

/// Nesting that a block level, or an `elif`/`else`, adds to the statements in it.
const STMT_WEIGHT: usize = 2;

/// Nesting tokens a `lambda` keyword counts for.
const LAMBDA_WEIGHT: usize = 2;

/// Checks that the syntax tree of `code` cannot be deeper than about
/// [`MAX_NESTING`], that brackets nest at most
/// [`MAX_BRACKET_DEPTH`] deep and that no more than [`MAX_UNARY_RUN`] unary operators follow
/// each other.
pub(crate) fn check(code: &str) -> Result<(), PrecheckError> {
    let mut walker = Walker {
        code,
        pos: 0,
        line: Level::default(),
        open: Vec::new(),
        blocks: Vec::new(),
        block_depth: 0,
        line_start: None,
        continued: false,
        unary_run: 0,
    };
    loop {
        if walker.in_fstring_text() {
            walker.fstring_text();
            continue;
        }
        let Some(token) = next_token(code, walker.pos) else {
            break;
        };
        walker.pos = token.end;
        walker.token(token)?;
    }
    while !walker.open.is_empty() {
        walker.close_level();
    }
    walker.end_line()
}

/// The logical line, an open bracket pair or an open f-string.
#[derive(Default)]
struct Level {
    /// The level is an f-string.
    fstring: Option<FString>,
    /// Nesting tokens in the current element.
    tokens: usize,
    /// Depth of the deepest level closed in the current element.
    child: usize,
    /// Depth of the deepest element that ended.
    deepest: usize,
    /// `lambda`s whose parameter list is open.
    lambdas: usize,
}

impl Level {
    fn element_depth(&self) -> usize {
        self.tokens.saturating_add(self.child)
    }

    fn end_element(&mut self) {
        self.deepest = self.deepest.max(self.element_depth());
        self.tokens = 0;
        self.child = 0;
        self.lambdas = 0;
    }

    /// Depth of the tree of everything in this level, including the current element.
    fn depth(&self) -> usize {
        self.deepest.max(self.element_depth())
    }
}

#[derive(Clone, Copy)]
struct FString {
    quote: Quote,
    /// In a `{...}` part rather than in the text.
    in_expr: bool,
}

/// A block level: statements indented by `indent`.
struct Block {
    indent: usize,
    /// `elif`s and `else`s so far in the current chain at this level.
    chain: usize,
}

struct Walker<'a> {
    code: &'a str,
    pos: usize,
    /// The logical line outside brackets.
    line: Level,
    /// The open bracket pairs and f-strings, innermost last.
    open: Vec<Level>,
    blocks: Vec<Block>,
    /// Sum of `1 + chain` over `blocks`.
    block_depth: usize,
    /// Where the current logical line starts, and how deeply its statement is nested.
    line_start: Option<(usize, usize)>,
    /// The previous token was a `\` that continues the logical line.
    continued: bool,
    unary_run: usize,
}

impl Walker<'_> {
    fn top(&mut self) -> &mut Level {
        match self.open.last_mut() {
            Some(level) => level,
            None => &mut self.line,
        }
    }

    fn in_fstring_text(&self) -> bool {
        matches!(
            self.open.last(),
            Some(Level {
                fstring: Some(FString { in_expr: false, .. }),
                ..
            })
        )
    }

    fn token(&mut self, token: Token) -> Result<(), PrecheckError> {
        let text = token.text(self.code);
        match token.kind {
            TokenKind::Newline => {
                if self.open.is_empty() && !std::mem::take(&mut self.continued) {
                    self.end_line()?;
                }
                return Ok(());
            }
            TokenKind::Comment => return Ok(()),
            TokenKind::Continuation => {
                self.continued = self.open.is_empty();
                return Ok(());
            }
            _ => {}
        }
        self.continued = false;

        let unary = match token.kind {
            TokenKind::Op => matches!(text, "-" | "+" | "~"),
            TokenKind::Keyword => text == "not",
            _ => false,
        };
        if unary {
            self.unary_run += 1;
            if self.unary_run > MAX_UNARY_RUN {
                return Err(PrecheckError::UnaryRun);
            }
        } else {
            self.unary_run = 0;
        }

        if self.open.is_empty() && self.line_start.is_none() {
            self.start_line(token);
        }

        match token.kind {
            TokenKind::Ident | TokenKind::Int | TokenKind::Float => {}
            TokenKind::Str(info) => {
                let prefix_end = token.start + usize::from(info.prefix_len);
                let prefix = self.code.get(token.start..prefix_end).unwrap_or("");
                if prefix.contains('f') {
                    // Starlark ends an f-string where its text ends, which is not where a plain
                    // string would end: scan it from just after its opening quotes.
                    self.top().tokens += 1;
                    self.pos = prefix_end + info.quote.len();
                    self.open_level(Some(FString {
                        quote: info.quote,
                        in_expr: false,
                    }))?;
                }
            }
            TokenKind::Comma => {
                let top = self.top();
                if top.lambdas == 0 {
                    top.end_element();
                }
            }
            TokenKind::Semicolon => {
                if self.open.is_empty() {
                    self.line.end_element();
                } else {
                    self.top().tokens += 1;
                }
            }
            TokenKind::Colon => {
                let top = self.top();
                top.tokens += 1;
                top.lambdas = top.lambdas.saturating_sub(1);
            }
            TokenKind::Keyword => {
                let top = self.top();
                if text == "lambda" {
                    top.tokens += LAMBDA_WEIGHT;
                    top.lambdas += 1;
                } else {
                    top.tokens += 1;
                }
            }
            TokenKind::Open(_) => {
                self.top().tokens += 1;
                self.open_level(None)?;
            }
            TokenKind::Close(bracket) => {
                let fstring = self
                    .open
                    .last_mut()
                    .and_then(|level| level.fstring.as_mut());
                match fstring {
                    // The end of a `{...}` part.
                    Some(fstring) => {
                        if bracket == Bracket::Curly {
                            fstring.in_expr = false;
                            self.top().end_element();
                        }
                        // Any other closing bracket is a syntax error.
                    }
                    None => self.close_level(),
                }
            }
            TokenKind::Dot
            | TokenKind::Assign
            | TokenKind::Arrow
            | TokenKind::Op
            | TokenKind::Error => self.top().tokens += 1,
            TokenKind::Newline | TokenKind::Comment | TokenKind::Continuation => {}
        }
        Ok(())
    }

    fn open_level(&mut self, fstring: Option<FString>) -> Result<(), PrecheckError> {
        self.open.push(Level {
            fstring,
            ..Level::default()
        });
        if self.open.len() > MAX_BRACKET_DEPTH {
            return Err(PrecheckError::TooDeep);
        }
        Ok(())
    }

    /// Closes the innermost open level (if any): it is one level deeper than what it contains.
    fn close_level(&mut self) {
        if let Some(level) = self.open.pop() {
            let depth = level.depth().saturating_add(1);
            let top = self.top();
            top.child = top.child.max(depth);
        }
    }

    /// Scans the text of the innermost f-string, which is open, from `self.pos`, up to its end
    /// (which closes it) or the start of a `{...}` part.
    fn fstring_text(&mut self) {
        let Some(FString { quote, .. }) = self.open.last().and_then(|level| level.fstring) else {
            return;
        };
        let bytes = self.code.as_bytes();
        loop {
            let Some(&b) = bytes.get(self.pos) else {
                // Unterminated: Starlark reports it.
                self.close_level();
                return;
            };
            let next = bytes.get(self.pos + 1).copied();
            match b {
                b'\\' => {
                    // An escape: the backslash takes the next character, even in a raw
                    // f-string.
                    self.pos += 1;
                    self.pos += self.char_len();
                }
                b'{' if next == Some(b'{') => self.pos += 2,
                b'{' => {
                    self.pos += 1;
                    if let Some(fstring) = self.open.last_mut().and_then(|l| l.fstring.as_mut()) {
                        fstring.in_expr = true;
                    }
                    return;
                }
                b'}' if next == Some(b'}') => self.pos += 2,
                b'\n' if !quote.is_triple() => {
                    // Unterminated: Starlark reports it. The newline is not part of it.
                    self.close_level();
                    return;
                }
                _ if b == quote.quote_byte() => {
                    let end = if quote.is_triple() {
                        self.code
                            .get(self.pos..)
                            .is_some_and(|rest| rest.starts_with(quote.delimiter()))
                    } else {
                        true
                    };
                    if end {
                        self.pos += quote.len();
                        self.close_level();
                        return;
                    }
                    self.pos += 1;
                }
                _ => self.pos += self.char_len(),
            }
        }
    }

    /// Length of the character at `self.pos`: 0 at the end, 1 off a character boundary.
    fn char_len(&self) -> usize {
        if self.pos >= self.code.len() {
            return 0;
        }
        self.code
            .get(self.pos..)
            .and_then(|rest| rest.chars().next())
            .map_or(1, char::len_utf8)
    }

    /// `token` starts a logical line: works out the blocks its statement is in.
    fn start_line(&mut self, token: Token) {
        let line_begin = self
            .code
            .get(..token.start)
            .and_then(|before| before.rfind('\n'))
            .map_or(0, |i| i + 1);
        let indent = token.start.saturating_sub(line_begin);
        while let Some(block) = self.blocks.last() {
            if block.indent <= indent {
                break;
            }
            self.block_depth = self.block_depth.saturating_sub(1 + block.chain);
            self.blocks.pop();
        }
        let chained =
            token.kind == TokenKind::Keyword && matches!(token.text(self.code), "elif" | "else");
        match self.blocks.last_mut() {
            Some(block) if block.indent == indent => {
                if chained {
                    // Nested in the `if` (or `elif`) before it.
                    block.chain += 1;
                    self.block_depth += 1;
                } else {
                    self.block_depth = self.block_depth.saturating_sub(block.chain);
                    block.chain = 0;
                }
            }
            _ => {
                self.blocks.push(Block { indent, chain: 0 });
                self.block_depth += 1;
            }
        }
        // The top level does not count.
        let stmt_depth = self.block_depth.saturating_sub(1);
        self.line_start = Some((token.start, stmt_depth.saturating_mul(STMT_WEIGHT)));
    }

    /// The logical line ended (nothing is open): checks its depth.
    fn end_line(&mut self) -> Result<(), PrecheckError> {
        let line = std::mem::take(&mut self.line);
        let Some((start, stmt_depth)) = self.line_start.take() else {
            return Ok(());
        };
        if stmt_depth.saturating_add(line.depth()) > MAX_NESTING {
            let line = self
                .code
                .get(..start)
                .map_or(0, |before| before.matches('\n').count())
                + 1;
            return Err(PrecheckError::TooComplex { line });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(code: &str) -> bool {
        check(code).is_ok()
    }

    fn too_complex(code: &str) -> bool {
        matches!(check(code), Err(PrecheckError::TooComplex { .. }))
    }

    /// `f(n)` for a range of `n` around the limit: small ones pass, big ones do not.
    fn assert_limit(f: impl Fn(usize) -> String, ok_up_to: usize, rejected_from: usize) {
        assert!(ok(&f(ok_up_to)), "{} should pass", f(ok_up_to));
        assert!(
            too_complex(&f(rejected_from)),
            "{} should be rejected",
            f(rejected_from)
        );
        // Much bigger ones are rejected too, and quickly.
        assert!(too_complex(&f(rejected_from * 20)));
    }

    #[test]
    fn test_chains() {
        let chain = |sep: &str, n: usize| format!("x = {}", vec!["1"; n].join(sep));
        // `x = ` counts one (`=`), each operator one.
        assert_limit(|n| chain("+", n), MAX_NESTING, MAX_NESTING + 1);
        assert_limit(|n| chain(" and ", n), MAX_NESTING, MAX_NESTING + 1);
        assert_limit(|n| chain(" | ", n), MAX_NESTING, MAX_NESTING + 1);
        assert_limit(|n| format!("x = y{}", ".a".repeat(n)), 199, 200);
        assert_limit(|n| format!("x = y{}", "[0]".repeat(n)), 198, 199);
        assert_limit(|n| format!("x = y{}", "()".repeat(n)), 198, 199);
        assert_limit(|n| format!("x = {}1", "1 if a else ".repeat(n)), 99, 100);
        assert_limit(|n| format!("x = {}1", "lambda: ".repeat(n)), 66, 67);
        assert_limit(|n| format!("x = {}1", "lambda a, b: ".repeat(n)), 66, 67);
    }

    #[test]
    fn test_elements_are_separate() {
        let chain = vec!["1"; 150].join("+");
        // Each element is shallow enough; together they would not be.
        assert!(ok(&format!("x = [{chain}, {chain}, {chain}]")));
        assert!(ok(&format!("x = {chain}, {chain}")));
        assert!(ok(&format!("f({chain}, y = {chain})")));
        assert!(ok(&format!("x = {chain}; y = {chain}")));
        assert!(ok(&format!("x = {chain}\ny = {chain}\n")));
        assert!(ok(&format!("x = {{'a': {chain}, 'b': {chain}}}")));
        // A long flat literal is fine.
        let items: Vec<String> = (0..100_000).map(|i| format!("-{i}")).collect();
        assert!(ok(&format!("x = [{}]", items.join(", "))));
        let entries: Vec<String> = (0..10_000).map(|i| format!("'k{i}': {i}")).collect();
        assert!(ok(&format!("x = {{{}}}", entries.join(", "))));
        // But not a lambda's parameters: its body is nested in it.
        let lambdas = "lambda a, b: ".repeat(60);
        assert!(too_complex(&format!("x = [{lambdas}{lambdas}1]")));
        assert!(ok(&format!("x = [{lambdas}1, {lambdas}1]")));
        // Nor, of course, brackets.
        let half = vec!["1"; 120].join("+");
        assert!(too_complex(&format!("x = {half} + ({half})")));
    }

    #[test]
    fn test_blocks() {
        let nested = |n: usize| {
            let mut code = String::new();
            for i in 0..n {
                code.push_str(&format!("{}if x:\n", " ".repeat(i)));
            }
            code.push_str(&format!("{}pass\n", " ".repeat(n)));
            code
        };
        // The `pass` line is in `n` blocks below the top level.
        assert_limit(nested, 99, 100);
        let elifs = |n: usize| format!("if x:\n    pass\n{}", "elif x:\n    pass\n".repeat(n));
        assert_limit(elifs, 98, 99);
        // A chain ends at the next statement at its level.
        let chain = "if x:\n    pass\n".to_owned() + &"elif x:\n    pass\n".repeat(90);
        assert!(ok(&format!("{chain}{chain}{chain}")));
        // Dedenting pops blocks.
        let deep = nested(90);
        assert!(ok(&format!("{deep}{deep}{deep}")));
        // Blank lines, comments and lines inside brackets or after `\` are not statements.
        assert!(ok(&format!(
            "def f():\n    x = [\n1,\n]\n\n# c\n    y = 1 + \\\n2\n    return x\n{}",
            nested(90)
        )));
    }

    #[test]
    fn test_statement_and_line_depths_add_up() {
        let mut code = String::new();
        for i in 0..50 {
            code.push_str(&format!("{}for x in y:\n", " ".repeat(i)));
        }
        let chain = vec!["1"; 150].join("+");
        assert!(ok(&format!("{code}{}x = 1\n", " ".repeat(50))));
        assert!(ok(&format!("{}x = {chain}\n", " ".repeat(50))));
        assert!(too_complex(&format!(
            "{code}{}x = {chain}\n",
            " ".repeat(50)
        )));
    }

    #[test]
    fn test_fstrings() {
        // Text is not counted; `{...}` parts are, like bracket pairs.
        let text = "a.b(c)+d ".repeat(100);
        assert!(ok(&format!("x = f'{text}{{x}}'")));
        assert!(ok(&format!("x = f'{{{{{text}}}}}'")));
        let chain = vec!["1"; 250].join("+");
        assert!(too_complex(&format!("x = f'{{{chain}}}'")));
        assert!(too_complex(&format!("x = f'a {{y}} b {{{chain}}}'")));
        // Expressions may hold strings with the f-string's own quotes, which would end a
        // plain string: the chain after them is still counted.
        assert!(too_complex(&format!(
            "x = f\"{{\"a\" + {chain} + \"b\"}}\""
        )));
        assert!(too_complex(&format!(
            "x = f'{{f'{{f'{{\"}}\" + {chain}}}'}}'}}'"
        )));
        assert!(ok("x = f\"{\"a\" + y}\" + f'{ {1: 2}[1] }'"));
        // Triple quotes, raw f-strings and escapes.
        assert!(too_complex(&format!("x = f'''a ' '' \n {{ {chain} }}'''")));
        assert!(too_complex(&format!("x = fr'\\' {{ {chain} }}'")));
        assert!(ok(&format!("x = f'\\{{ {chain} }}'"))); // `\{` is text
        // Nested f-strings count as brackets.
        let nested = |n: usize| format!("x = {}1{}", "f'{".repeat(n), "}'".repeat(n));
        assert!(ok(&nested(MAX_BRACKET_DEPTH)));
        assert_eq!(
            check(&nested(MAX_BRACKET_DEPTH + 1)),
            Err(PrecheckError::TooDeep)
        );
        // Unterminated f-strings end at the end of the line (single quotes) or input.
        assert!(ok("x = f'{y\nz = 1"));
        assert!(ok("x = f'abc\nz = 1"));
        assert!(ok("x = f'''abc\nz = 1"));
        assert!(ok("x = f'{"));
        assert!(ok("x = f'"));
        // Newlines inside a `{...}` part do not end the statement.
        assert!(too_complex(&format!("x = f'{{\n{chain}\n}}'")));
    }

    #[test]
    fn test_line_numbers() {
        let chain = vec!["1"; 250].join("+");
        assert_eq!(
            check(&format!("x = 1\n\n# c\ny = [\n  {chain},\n]\n")),
            Err(PrecheckError::TooComplex { line: 4 })
        );
        assert_eq!(check(&chain), Err(PrecheckError::TooComplex { line: 1 }));
    }

    #[test]
    fn test_never_panics() {
        let samples = [
            "",
            "\n",
            "\\",
            "\\\n",
            ")",
            "]]]",
            "}",
            "f'",
            "f'{",
            "f'{)}'",
            "f'}'",
            "f'\\",
            "f'''",
            "f\"{'}'}\"",
            "rb'",
            "fr'\\",
            "lambda",
            "lambda:",
            ":::",
            ";;",
            "elif",
            "else:\n  else:\n else:",
            "  x\ny\n    z\n  w",
            "é",
            "f'é{é}é'",
            "\u{0}",
            "x = (lambda a, b: [c, lambda: d])",
            "if x:\n\n    \n  y\nelse:\n  z",
        ];
        for s in samples {
            let _ignored = check(s);
            for i in 0..=s.len() {
                if let Some(prefix) = s.get(..i) {
                    let _ignored = check(prefix);
                }
            }
        }
    }
}
