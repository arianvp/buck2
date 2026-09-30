/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! A tolerant Starlark tokenizer.
//!
//! It never fails: characters it does not understand become [`TokenKind::Error`] tokens, and
//! unterminated strings become string tokens with `closed: false`. Whitespace is not tokenized
//! (a token's span says where it is), but newlines are, since they end statements.
//!
//! Lexing can resume in the middle of a string literal (see [`lex_from`]), so that input can
//! be lexed line by line.

use std::ops::Range;

/// Starlark keywords.
pub const KEYWORDS: &[&str] = &[
    "and", "break", "continue", "def", "elif", "else", "for", "if", "in", "lambda", "load", "not",
    "or", "pass", "return",
];

/// Words that Starlark reserves (from Python) and rejects as identifiers.
pub const RESERVED: &[&str] = &[
    "as", "assert", "async", "await", "class", "del", "except", "finally", "from", "global",
    "import", "is", "nonlocal", "raise", "try", "while", "with", "yield",
];

/// Keywords that start a compound statement (a block header).
pub const BLOCK_KEYWORDS: &[&str] = &["def", "elif", "else", "for", "if"];

/// The letters that may prefix a string literal, as Starlark accepts them.
const STRING_PREFIXES: &[&str] = &["r", "b", "f", "br", "rb", "fr"];

/// Operators, longest first so that the first match is the longest one. `=` and `->` are not
/// here: they have token kinds of their own.
const OPERATORS: &[&str] = &[
    "//=", "<<=", ">>=", "...", "**", "//", "<<", ">>", "<=", ">=", "!=", "==", "+=", "-=", "*=",
    "/=", "%=", "&=", "|=", "^=", "+", "-", "*", "/", "%", "&", "|", "^", "~", "<", ">",
];

pub fn is_keyword(word: &str) -> bool {
    KEYWORDS.contains(&word)
}

pub fn is_reserved(word: &str) -> bool {
    RESERVED.contains(&word)
}

pub fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

pub fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The leading identifier-like word of `s` (possibly empty).
pub fn leading_word(s: &str) -> &str {
    let len = s
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
        .count();
    s.get(..len).unwrap_or("")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Quote {
    /// `'`
    Single,
    /// `"`
    Double,
    /// `'''`
    TripleSingle,
    /// `"""`
    TripleDouble,
}

impl Quote {
    pub fn is_triple(self) -> bool {
        matches!(self, Quote::TripleSingle | Quote::TripleDouble)
    }

    pub fn delimiter(self) -> &'static str {
        match self {
            Quote::Single => "'",
            Quote::Double => "\"",
            Quote::TripleSingle => "'''",
            Quote::TripleDouble => "\"\"\"",
        }
    }

    /// Length of the delimiter in bytes.
    pub fn len(self) -> usize {
        self.delimiter().len()
    }

    /// The quote character.
    pub fn quote_byte(self) -> u8 {
        match self {
            Quote::Single | Quote::TripleSingle => b'\'',
            Quote::Double | Quote::TripleDouble => b'"',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bracket {
    /// `(` `)`
    Paren,
    /// `[` `]`
    Square,
    /// `{` `}`
    Curly,
}

impl Bracket {
    pub fn open(self) -> char {
        match self {
            Bracket::Paren => '(',
            Bracket::Square => '[',
            Bracket::Curly => '{',
        }
    }

    pub fn close(self) -> char {
        match self {
            Bracket::Paren => ')',
            Bracket::Square => ']',
            Bracket::Curly => '}',
        }
    }
}

/// Details of a string literal token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StrInfo {
    pub quote: Quote,
    /// `r"..."`: backslashes are kept literally.
    pub raw: bool,
    /// The closing delimiter is present.
    pub closed: bool,
    /// The token starts inside a string that was opened before the lexed text (see
    /// [`lex_from`]): it has neither prefix nor opening delimiter.
    pub continued: bool,
    /// Length in bytes of the prefix letters (`r`, `b`, `f`, `rb`, ...).
    pub prefix_len: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenKind {
    Ident,
    /// A keyword, or a reserved word.
    Keyword,
    Int,
    Float,
    Str(StrInfo),
    Open(Bracket),
    Close(Bracket),
    Comma,
    Dot,
    Colon,
    Semicolon,
    /// `=`
    Assign,
    /// `->`
    Arrow,
    /// Any other operator, including augmented assignments such as `+=`.
    Op,
    /// From `#` to the end of the line (the newline is not included).
    Comment,
    Newline,
    /// A `\` that continues the line: it is followed by a newline or the end of the input.
    Continuation,
    /// A character that cannot start a token.
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Token {
    pub kind: TokenKind,
    /// Byte offset of the first byte.
    pub start: usize,
    /// Byte offset past the last byte.
    pub end: usize,
}

impl Token {
    pub fn span(&self) -> Range<usize> {
        self.start..self.end
    }

    /// The token's text. Empty if `src` is not the text the token came from.
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        src.get(self.start..self.end).unwrap_or("")
    }

    /// For a string token, the byte range of its content: between the delimiters, or up to the
    /// end of the token when it is not closed.
    pub fn str_content(&self) -> Option<Range<usize>> {
        match self.kind {
            TokenKind::Str(info) => {
                let open = if info.continued {
                    0
                } else {
                    usize::from(info.prefix_len) + info.quote.len()
                };
                let close = if info.closed { info.quote.len() } else { 0 };
                let start = (self.start + open).min(self.end);
                let end = self.end.saturating_sub(close).max(start);
                Some(start..end)
            }
            _ => None,
        }
    }

    /// Whether this is the keyword (or reserved word) `word`.
    pub fn is_keyword(&self, src: &str, word: &str) -> bool {
        self.kind == TokenKind::Keyword && self.text(src) == word
    }

    /// Whether this is the operator `op`.
    pub fn is_op(&self, src: &str, op: &str) -> bool {
        self.kind == TokenKind::Op && self.text(src) == op
    }
}

/// A string literal that is still open at the end of the lexed text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpenString {
    pub quote: Quote,
    pub raw: bool,
}

/// Lexer state at a line boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct LexState {
    /// The text ended inside a string literal that continues on the next line: a triple-quoted
    /// string, or a single-quoted one whose last character is an escaping backslash.
    pub open_string: Option<OpenString>,
}

/// Tokenizes `src`.
pub fn lex(src: &str) -> Vec<Token> {
    lex_from(src, LexState::default()).0
}

/// Tokenizes `src`, starting in `state`, and returns the tokens and the state at the end.
///
/// Lexing a text line by line, passing each line's end state to the next line, gives the same
/// tokens as lexing the whole text, except that a string spanning lines is split into one token
/// per line (the later ones `continued`).
pub fn lex_from(src: &str, state: LexState) -> (Vec<Token>, LexState) {
    let mut lexer = Lexer {
        src,
        bytes: src.as_bytes(),
        pos: 0,
        tokens: Vec::new(),
        open_string: None,
    };
    if let Some(open) = state.open_string {
        lexer.string_body(0, 0, open.quote, open.raw, 0, true);
    }
    lexer.run();
    (
        lexer.tokens,
        LexState {
            open_string: lexer.open_string,
        },
    )
}

/// The first token that starts at or after byte `pos` of `src`, lexed as if `pos` were not
/// inside a string literal. `None` if there is none.
///
/// This lets a caller lex a text piece by piece, e.g. to resume after a construct it scans
/// itself. `pos` should be on a character boundary.
pub fn next_token(src: &str, pos: usize) -> Option<Token> {
    let mut lexer = Lexer {
        src,
        bytes: src.as_bytes(),
        pos,
        tokens: Vec::new(),
        open_string: None,
    };
    while lexer.tokens.is_empty() && lexer.pos < lexer.bytes.len() {
        lexer.step();
    }
    lexer.tokens.pop()
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    tokens: Vec<Token>,
    open_string: Option<OpenString>,
}

impl Lexer<'_> {
    fn push(&mut self, kind: TokenKind, start: usize, end: usize) {
        self.tokens.push(Token { kind, start, end });
        self.pos = end;
    }

    /// The bytes from `i` on (empty if `i` is past the end).
    fn rest(&self, i: usize) -> &[u8] {
        self.bytes.get(i..).unwrap_or_default()
    }

    fn byte(&self, i: usize) -> Option<u8> {
        self.bytes.get(i).copied()
    }

    fn starts_with(&self, i: usize, s: &str) -> bool {
        self.bytes
            .get(i..)
            .is_some_and(|rest| rest.starts_with(s.as_bytes()))
    }

    /// Length of the character at byte `i` (1 if `i` is not on a character boundary).
    fn char_len(&self, i: usize) -> usize {
        self.src
            .get(i..)
            .and_then(|s| s.chars().next())
            .map_or(1, |c| c.len_utf8())
    }

    fn run(&mut self) {
        while self.pos < self.bytes.len() {
            self.step();
        }
    }

    /// Lexes from `self.pos`: skips one whitespace character or pushes one token. Either way
    /// `self.pos` moves forward, unless it is at the end.
    fn step(&mut self) {
        let start = self.pos;
        if let Some(b) = self.byte(start) {
            match b {
                b' ' | b'\t' | b'\r' | b'\x0c' => self.pos += 1,
                b'\n' => self.push(TokenKind::Newline, start, start + 1),
                b'#' => {
                    let end = self
                        .rest(start)
                        .iter()
                        .position(|b| *b == b'\n')
                        .map_or(self.bytes.len(), |i| start + i);
                    self.push(TokenKind::Comment, start, end);
                }
                b'\\' => {
                    let continues = match self.byte(start + 1) {
                        None | Some(b'\n') => true,
                        Some(b'\r') => matches!(self.byte(start + 2), None | Some(b'\n')),
                        Some(_) => false,
                    };
                    let kind = if continues {
                        TokenKind::Continuation
                    } else {
                        TokenKind::Error
                    };
                    self.push(kind, start, start + 1);
                }
                b'"' | b'\'' => self.string(start, 0),
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                    let len = self
                        .rest(start)
                        .iter()
                        .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
                        .count();
                    let end = start + len;
                    let word = self.src.get(start..end).unwrap_or("");
                    if matches!(self.byte(end), Some(b'"' | b'\''))
                        && STRING_PREFIXES.contains(&word)
                    {
                        self.string(start, len);
                    } else if is_keyword(word) || is_reserved(word) {
                        self.push(TokenKind::Keyword, start, end);
                    } else {
                        self.push(TokenKind::Ident, start, end);
                    }
                }
                b'0'..=b'9' => self.number(start),
                b'.' => {
                    if self.byte(start + 1).is_some_and(|b| b.is_ascii_digit()) {
                        self.number(start);
                    } else if self.starts_with(start, "...") {
                        self.push(TokenKind::Op, start, start + 3);
                    } else {
                        self.push(TokenKind::Dot, start, start + 1);
                    }
                }
                b'(' => self.push(TokenKind::Open(Bracket::Paren), start, start + 1),
                b'[' => self.push(TokenKind::Open(Bracket::Square), start, start + 1),
                b'{' => self.push(TokenKind::Open(Bracket::Curly), start, start + 1),
                b')' => self.push(TokenKind::Close(Bracket::Paren), start, start + 1),
                b']' => self.push(TokenKind::Close(Bracket::Square), start, start + 1),
                b'}' => self.push(TokenKind::Close(Bracket::Curly), start, start + 1),
                b',' => self.push(TokenKind::Comma, start, start + 1),
                b';' => self.push(TokenKind::Semicolon, start, start + 1),
                b':' => self.push(TokenKind::Colon, start, start + 1),
                b'=' if self.byte(start + 1) != Some(b'=') => {
                    self.push(TokenKind::Assign, start, start + 1)
                }
                b'-' if self.byte(start + 1) == Some(b'>') => {
                    self.push(TokenKind::Arrow, start, start + 2)
                }
                _ => match OPERATORS.iter().find(|op| self.starts_with(start, op)) {
                    Some(op) => self.push(TokenKind::Op, start, start + op.len()),
                    None => {
                        let len = self.char_len(start);
                        self.push(TokenKind::Error, start, start + len);
                    }
                },
            }
        }
    }

    /// Lexes a number starting at `start` (a digit, or a `.` followed by a digit).
    fn number(&mut self, start: usize) {
        let digits = |lexer: &Self, from: usize, pred: fn(&u8) -> bool| -> usize {
            from + lexer.rest(from).iter().take_while(|b| pred(b)).count()
        };
        if self.byte(start) == Some(b'0') {
            let radix_digits: Option<fn(&u8) -> bool> = match self.byte(start + 1) {
                Some(b'x' | b'X') => Some(u8::is_ascii_hexdigit),
                Some(b'o' | b'O') => Some(|b| (b'0'..=b'7').contains(b)),
                Some(b'b' | b'B') => Some(|b| *b == b'0' || *b == b'1'),
                _ => None,
            };
            if let Some(pred) = radix_digits {
                let end = digits(self, start + 2, pred);
                self.push(TokenKind::Int, start, end);
                return;
            }
        }
        let mut end = digits(self, start, u8::is_ascii_digit);
        let mut float = false;
        if self.byte(end) == Some(b'.') {
            float = true;
            end = digits(self, end + 1, u8::is_ascii_digit);
        }
        if matches!(self.byte(end), Some(b'e' | b'E')) {
            let exponent_digits = match self.byte(end + 1) {
                Some(b'+' | b'-') => end + 2,
                _ => end + 1,
            };
            if self
                .byte(exponent_digits)
                .is_some_and(|b| b.is_ascii_digit())
            {
                float = true;
                end = digits(self, exponent_digits, u8::is_ascii_digit);
            }
        }
        let kind = if float {
            TokenKind::Float
        } else {
            TokenKind::Int
        };
        self.push(kind, start, end);
    }

    /// Lexes a string literal whose prefix letters start at `start` (`prefix_len` of them).
    fn string(&mut self, start: usize, prefix_len: usize) {
        let prefix = self.src.get(start..start + prefix_len).unwrap_or("");
        let raw = prefix.contains('r');
        let open = start + prefix_len;
        let quote = match (
            self.starts_with(open, "\"\"\""),
            self.starts_with(open, "'''"),
        ) {
            (true, _) => Quote::TripleDouble,
            (_, true) => Quote::TripleSingle,
            _ if self.byte(open) == Some(b'"') => Quote::Double,
            _ => Quote::Single,
        };
        let prefix_len = u8::try_from(prefix_len).unwrap_or(u8::MAX);
        self.string_body(start, open + quote.len(), quote, raw, prefix_len, false);
    }

    /// Lexes the rest of a string literal from `body` (just after the opening delimiter) and
    /// pushes one token from `start`.
    fn string_body(
        &mut self,
        start: usize,
        body: usize,
        quote: Quote,
        raw: bool,
        prefix_len: u8,
        continued: bool,
    ) {
        let q = quote.quote_byte();
        let triple = quote.is_triple();
        let mut i = body;
        let (end, closed) = loop {
            let Some(b) = self.byte(i) else {
                // The text ended inside the string. A triple-quoted string continues on the
                // next line; so does a single-quoted one whose last character escapes the
                // newline.
                if triple {
                    self.open_string = Some(OpenString { quote, raw });
                }
                break (self.bytes.len(), false);
            };
            match b {
                b'\\' => {
                    if i + 1 >= self.bytes.len() {
                        self.open_string = Some(OpenString { quote, raw });
                        break (self.bytes.len(), false);
                    }
                    // A backslash always takes the next character with it, even in a raw
                    // string (`r"\""` is one string).
                    i += 1 + self.char_len(i + 1);
                }
                b'\n' if !triple => break (i, false),
                _ if b == q => {
                    if !triple {
                        break (i + 1, true);
                    }
                    if self.starts_with(i, quote.delimiter()) {
                        break (i + 3, true);
                    }
                    i += 1;
                }
                _ => i += self.char_len(i),
            }
        };
        self.push(
            TokenKind::Str(StrInfo {
                quote,
                raw,
                closed,
                continued,
                prefix_len,
            }),
            start,
            end.min(self.bytes.len()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        lex(src).into_iter().map(|t| t.kind).collect()
    }

    fn texts(src: &str) -> Vec<&str> {
        lex(src).into_iter().map(|t| t.text(src)).collect()
    }

    fn str_info(t: &Token) -> StrInfo {
        match t.kind {
            TokenKind::Str(info) => info,
            k => panic!("not a string: {k:?}"),
        }
    }

    #[test]
    fn test_simple_tokens() {
        assert_eq!(
            texts("x = foo(1, 2.5)[0].bar  # hi"),
            vec![
                "x", "=", "foo", "(", "1", ",", "2.5", ")", "[", "0", "]", ".", "bar", "# hi"
            ]
        );
        assert_eq!(
            kinds("def f(a) -> int:\n    return not a"),
            vec![
                TokenKind::Keyword,
                TokenKind::Ident,
                TokenKind::Open(Bracket::Paren),
                TokenKind::Ident,
                TokenKind::Close(Bracket::Paren),
                TokenKind::Arrow,
                TokenKind::Ident,
                TokenKind::Colon,
                TokenKind::Newline,
                TokenKind::Keyword,
                TokenKind::Keyword,
                TokenKind::Ident,
            ]
        );
    }

    #[test]
    fn test_operators() {
        assert_eq!(
            texts("a //= b ** c != d == e <= f >> g ... h"),
            vec![
                "a", "//=", "b", "**", "c", "!=", "d", "==", "e", "<=", "f", ">>", "g", "...", "h"
            ]
        );
        assert_eq!(
            kinds("a = -~b; c += 1"),
            vec![
                TokenKind::Ident,
                TokenKind::Assign,
                TokenKind::Op,
                TokenKind::Op,
                TokenKind::Ident,
                TokenKind::Semicolon,
                TokenKind::Ident,
                TokenKind::Op,
                TokenKind::Int,
            ]
        );
    }

    #[test]
    fn test_numbers() {
        assert_eq!(
            kinds("1 0x1F 0o17 0b101 1. .5 1e3 1.5e-3 7e"),
            vec![
                TokenKind::Int,
                TokenKind::Int,
                TokenKind::Int,
                TokenKind::Int,
                TokenKind::Float,
                TokenKind::Float,
                TokenKind::Float,
                TokenKind::Float,
                TokenKind::Int,
                TokenKind::Ident,
            ]
        );
        assert_eq!(texts("1.x"), vec!["1.", "x"]);
        assert_eq!(texts("ctx.a"), vec!["ctx", ".", "a"]);
    }

    #[test]
    fn test_keywords() {
        let src = "load lambda while iffy None";
        assert_eq!(
            kinds(src),
            vec![
                TokenKind::Keyword,
                TokenKind::Keyword,
                TokenKind::Keyword,
                TokenKind::Ident,
                TokenKind::Ident,
            ]
        );
        assert!(lex(src)[0].is_keyword(src, "load"));
        assert!(is_keyword("def"));
        assert!(!is_keyword("while"));
        assert!(is_reserved("while"));
        assert_eq!(leading_word("else:"), "else");
        assert_eq!(leading_word(":x"), "");
    }

    #[test]
    fn test_strings() {
        let src = r#"'a' "b\"c" r'\d' b"x" rb'y' f"{z}" 'it\'s'"#;
        let tokens = lex(src);
        assert_eq!(
            texts(src),
            vec![
                "'a'",
                r#""b\"c""#,
                r"r'\d'",
                r#"b"x""#,
                "rb'y'",
                r#"f"{z}""#,
                r"'it\'s'"
            ]
        );
        for t in &tokens {
            assert!(str_info(t).closed, "{:?}", t.text(src));
        }
        assert!(str_info(&tokens[2]).raw);
        assert!(!str_info(&tokens[1]).raw);
        assert_eq!(str_info(&tokens[4]).prefix_len, 2);
        let content = tokens[1].str_content().unwrap();
        assert_eq!(&src[content], r#"b\"c"#);
        // Not a prefix: an identifier followed by a string.
        assert_eq!(texts("x'a'"), vec!["x", "'a'"]);
    }

    #[test]
    fn test_unterminated_strings() {
        // A single-quoted string ends at the end of the line.
        let src = "print(\"hello wo\nx";
        let tokens = lex(src);
        assert_eq!(texts(src), vec!["print", "(", "\"hello wo", "\n", "x"]);
        let info = str_info(&tokens[2]);
        assert!(!info.closed);
        assert_eq!(&src[tokens[2].str_content().unwrap()], "hello wo");
        assert_eq!(lex_from(src, LexState::default()).1, LexState::default());

        // A triple-quoted string does not.
        let src = "x = \"\"\"abc\n\ndef";
        let (tokens, state) = lex_from(src, LexState::default());
        assert_eq!(tokens.len(), 3);
        assert!(!str_info(&tokens[2]).closed);
        assert_eq!(
            state.open_string,
            Some(OpenString {
                quote: Quote::TripleDouble,
                raw: false
            })
        );

        // Neither does a string whose last character is a backslash.
        let (_, state) = lex_from("x = 'abc\\", LexState::default());
        assert_eq!(state.open_string.map(|s| s.quote), Some(Quote::Single));
    }

    #[test]
    fn test_triple_quotes() {
        let src = "'''a ' '' \\''' b''' + \"\"\"c\"\"\"";
        let tokens = lex(src);
        assert_eq!(
            texts(src),
            vec!["'''a ' '' \\''' b'''", "+", "\"\"\"c\"\"\""]
        );
        assert!(str_info(&tokens[0]).closed);
        assert_eq!(str_info(&tokens[0]).quote, Quote::TripleSingle);
        assert_eq!(&src[tokens[2].str_content().unwrap()], "c");
        // An empty string is not the start of a triple-quoted one.
        assert_eq!(texts("'' + x"), vec!["''", "+", "x"]);
    }

    #[test]
    fn test_resume_in_string() {
        let (tokens, state) = lex_from("x = '''a", LexState::default());
        assert_eq!(tokens.len(), 3);
        let (tokens, state) = lex_from("b", state);
        assert_eq!(tokens.len(), 1);
        assert!(str_info(&tokens[0]).continued);
        assert!(state.open_string.is_some());
        let line = "c''' + f(";
        let (tokens, state) = lex_from(line, state);
        assert_eq!(texts_of(line, &tokens), vec!["c'''", "+", "f", "("]);
        let info = str_info(&tokens[0]);
        assert!(info.closed && info.continued);
        assert_eq!(&line[tokens[0].str_content().unwrap()], "c");
        assert_eq!(state, LexState::default());
    }

    fn texts_of<'a>(src: &'a str, tokens: &[Token]) -> Vec<&'a str> {
        tokens.iter().map(|t| t.text(src)).collect()
    }

    #[test]
    fn test_comments() {
        assert_eq!(texts("x # a 'b' (\ny"), vec!["x", "# a 'b' (", "\n", "y"]);
        assert_eq!(texts("'#' # c"), vec!["'#'", "# c"]);
    }

    #[test]
    fn test_brackets() {
        assert_eq!(
            kinds("([{}])"),
            vec![
                TokenKind::Open(Bracket::Paren),
                TokenKind::Open(Bracket::Square),
                TokenKind::Open(Bracket::Curly),
                TokenKind::Close(Bracket::Curly),
                TokenKind::Close(Bracket::Square),
                TokenKind::Close(Bracket::Paren),
            ]
        );
        assert_eq!(Bracket::Square.open(), '[');
        assert_eq!(Bracket::Curly.close(), '}');
    }

    #[test]
    fn test_continuation_and_errors() {
        assert_eq!(
            kinds("x = 1 + \\\n2"),
            vec![
                TokenKind::Ident,
                TokenKind::Assign,
                TokenKind::Int,
                TokenKind::Op,
                TokenKind::Continuation,
                TokenKind::Newline,
                TokenKind::Int,
            ]
        );
        assert_eq!(
            kinds("x \\"),
            vec![TokenKind::Ident, TokenKind::Continuation]
        );
        assert_eq!(
            kinds("a \\ b"),
            vec![TokenKind::Ident, TokenKind::Error, TokenKind::Ident]
        );
        assert_eq!(texts("$ é ! `"), vec!["$", "é", "!", "`"]);
        assert!(lex("$ é ! `").iter().all(|t| t.kind == TokenKind::Error));
    }

    #[test]
    fn test_never_panics() {
        let samples = [
            "",
            "'",
            "\"",
            "'''",
            "\"\"\"",
            "\\",
            "r'",
            "rb\"",
            "0x",
            "0b",
            ".",
            "1e",
            "1e+",
            "é'é",
            "'é\\",
            "'\\é'",
            "\"\"\"é\\",
            "#",
            "\r",
            "\\\r",
            "a\\\r\n",
            "\u{0}",
        ];
        for s in samples {
            let tokens = lex(s);
            for t in &tokens {
                assert!(t.start <= t.end && t.end <= s.len(), "{s:?}: {t:?}");
                assert!(s.get(t.start..t.end).is_some(), "{s:?}: {t:?}");
                if let Some(r) = t.str_content() {
                    assert!(s.get(r).is_some(), "{s:?}: {t:?}");
                }
            }
            assert_eq!(tokens_one_by_one(s), tokens, "{s:?}");
        }
    }

    /// Lexes `src` with [`next_token`], resuming after each token.
    fn tokens_one_by_one(src: &str) -> Vec<Token> {
        let mut tokens = Vec::new();
        let mut pos = 0;
        while let Some(t) = next_token(src, pos) {
            assert!(t.end > pos, "{src:?}: {t:?}");
            pos = t.end;
            tokens.push(t);
        }
        tokens
    }

    #[test]
    fn test_next_token() {
        let src = "x = f(1, 'a\\'')  # c\n  y.z[2] \\\n\"\"\"doc\n\"\"\"";
        assert_eq!(tokens_one_by_one(src), lex(src));
        assert_eq!(next_token(src, src.len()), None);
        assert_eq!(next_token(src, src.len() + 5), None);
        assert_eq!(next_token("   ", 0), None);
        // It starts outside any string: the rest of a string lexes as code.
        let t = next_token("'ab cd'", 4);
        assert_eq!(
            t.map(|t| (t.kind, t.start, t.end)),
            Some((TokenKind::Ident, 4, 6))
        );
    }
}
