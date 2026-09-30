/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Where the cursor is in a (partial) query, for completion.
//!
//! The query parser rejects partial input, so [`scan_query`] scans the text before the cursor
//! tolerantly, the way the query grammar splits it: words (`[A-Za-z0-9*/@._:$#%-]+`, or quoted),
//! calls `name(arg, arg)`, `set(word word)`, parentheses and the binary operators (`+`, `-`,
//! `^` and `union`, `except`, `intersect` between spaces). It finds the word being typed, the
//! innermost call it is an argument of, and whether an operator could be typed there.

/// The binary operators written as words. They are functions in the query environment's
/// description too, but cannot be called as functions.
pub const OPERATOR_WORDS: &[&str] = &["except", "intersect", "union"];

/// Calls that the query grammar parses itself: their arguments are words separated by spaces
/// (target patterns for `set`, files for `fileset`).
pub const LITERAL_CALLS: &[&str] = &["fileset", "set"];

/// What an argument of a query function takes, as the daemon describes the functions to the
/// client: the codes of a function's arguments, comma-separated, are the `detail` of its
/// completion candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryArg {
    /// A string (a regex, an attribute name, ...).
    String,
    /// An integer (a depth).
    Integer,
    /// Targets (or files): target patterns, or an expression.
    Targets,
    /// Files: paths, or an expression.
    Files,
    /// Any expression (e.g. the captures of `deps`).
    Expression,
}

impl QueryArg {
    const ALL: [QueryArg; 5] = [
        QueryArg::String,
        QueryArg::Integer,
        QueryArg::Targets,
        QueryArg::Files,
        QueryArg::Expression,
    ];

    pub fn code(self) -> &'static str {
        match self {
            QueryArg::String => "string",
            QueryArg::Integer => "integer",
            QueryArg::Targets => "targets",
            QueryArg::Files => "files",
            QueryArg::Expression => "expression",
        }
    }

    pub fn from_code(code: &str) -> Option<QueryArg> {
        Self::ALL.into_iter().find(|arg| arg.code() == code)
    }
}

/// The detail of the candidate of a function with these arguments.
pub fn encode_query_args(args: impl IntoIterator<Item = QueryArg>) -> String {
    let codes: Vec<&str> = args.into_iter().map(QueryArg::code).collect();
    codes.join(",")
}

/// The arguments of a function from the detail of its candidate (`None` for an unknown code).
pub fn decode_query_args(detail: &str) -> Vec<Option<QueryArg>> {
    if detail.is_empty() {
        return Vec::new();
    }
    detail.split(',').map(QueryArg::from_code).collect()
}

/// The cursor in a query.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QueryContext<'a> {
    /// Byte offset of the start of the word being typed, in the query text.
    pub word_start: usize,
    /// The word being typed (the content of a quoted word, without its quote).
    pub word: &'a str,
    /// The word is in quotes that are not closed yet.
    pub quoted: bool,
    /// The innermost call the word is an argument of: the function's name (or `set`,
    /// `fileset`), and the index of the argument (the commas before it).
    pub call: Option<(&'a str, usize)>,
    /// The word follows a complete operand: a binary operator is expected here.
    pub operator: bool,
}

/// Whether `c` may be part of an unquoted query word.
pub fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "*/@._:$#%-".contains(c)
}

/// Whether the unquoted query word `word` is a target pattern (or a file path) rather than the
/// start of a function name.
pub fn is_pattern_word(word: &str) -> bool {
    word.contains(['/', ':', '@', '.', '*', '$', '#', '%'])
}

/// The cursor at the end of `text`, the query before it.
pub fn scan_query(text: &str) -> QueryContext<'_> {
    // The unclosed calls and parentheses: the function's name (`None` for a parenthesis), and
    // the index of the argument.
    let mut stack: Vec<(Option<&str>, usize)> = Vec::new();
    // The start of the unquoted word being scanned.
    let mut word_start: Option<usize> = None;
    // The start of the content of an unclosed quoted word, and its quote.
    let mut quote: Option<(usize, char)> = None;
    // The last thing scanned is a complete operand (so an operator may follow).
    let mut after_operand = false;
    // `after_operand` where the current word (or quoted word) started.
    let mut operator_at_word = false;

    for (i, c) in text.char_indices() {
        if let Some((_, q)) = quote {
            if c == q {
                quote = None;
                after_operand = true;
            }
            continue;
        }
        if is_word_char(c) {
            if word_start.is_none() {
                word_start = Some(i);
                operator_at_word = after_operand;
            }
            continue;
        }
        // The end of a word.
        let word = word_start.take().and_then(|start| text.get(start..i));
        match (word, c) {
            (Some(name), '(') => {
                stack.push((Some(name), 0));
                after_operand = false;
                continue;
            }
            (Some(word), _) => {
                after_operand = !(word == "-" || OPERATOR_WORDS.contains(&word));
            }
            (None, _) => {}
        }
        match c {
            '(' => {
                stack.push((None, 0));
                after_operand = false;
            }
            ')' => {
                stack.pop();
                after_operand = true;
            }
            ',' => {
                if let Some((_, arg)) = stack.last_mut() {
                    *arg = arg.saturating_add(1);
                }
                after_operand = false;
            }
            '"' | '\'' => {
                operator_at_word = after_operand;
                quote = Some((i + c.len_utf8(), c));
            }
            c if c.is_whitespace() => {}
            // `+`, `^`, and anything the grammar does not know.
            _ => after_operand = false,
        }
    }

    let call = match stack.last() {
        Some((Some(name), arg)) => Some((*name, *arg)),
        _ => None,
    };
    let in_literal_call = call.is_some_and(|(name, _)| LITERAL_CALLS.contains(&name));
    let (word_start, quoted, operator) = match (quote, word_start) {
        (Some((start, _)), _) => (start, true, operator_at_word),
        (None, Some(start)) => (start, false, operator_at_word),
        (None, None) => (text.len(), false, after_operand),
    };
    QueryContext {
        word_start,
        word: text.get(word_start..).unwrap_or(""),
        quoted,
        call,
        // The words of `set(a b c)` are separated by spaces.
        operator: operator && !in_literal_call,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(
        word_start: usize,
        word: &'static str,
        call: Option<(&'static str, usize)>,
        operator: bool,
    ) -> QueryContext<'static> {
        QueryContext {
            word_start,
            word,
            quoted: false,
            call,
            operator,
        }
    }

    #[test]
    fn test_functions_and_words() {
        assert_eq!(scan_query(""), ctx(0, "", None, false));
        assert_eq!(scan_query("dep"), ctx(0, "dep", None, false));
        assert_eq!(
            scan_query("deps(//li"),
            ctx(5, "//li", Some(("deps", 0)), false)
        );
        assert_eq!(scan_query("deps("), ctx(5, "", Some(("deps", 0)), false));
        assert_eq!(
            scan_query("deps(//a, 1"),
            ctx(10, "1", Some(("deps", 1)), false)
        );
        assert_eq!(
            scan_query("deps(//a,"),
            ctx(9, "", Some(("deps", 1)), false)
        );
        assert_eq!(
            scan_query("rdeps(//..., deps(:x), fi"),
            ctx(23, "fi", Some(("rdeps", 2)), false)
        );
        assert_eq!(
            scan_query("kind(\"cxx.*\", deps("),
            ctx(19, "", Some(("deps", 0)), false)
        );
        assert_eq!(scan_query("deps(//a)"), ctx(9, "", None, true));
        assert_eq!(scan_query("f(g(x), "), ctx(8, "", Some(("f", 1)), false));
    }

    #[test]
    fn test_operators() {
        assert_eq!(scan_query("deps(//a) "), ctx(10, "", None, true));
        assert_eq!(scan_query("deps(//a) un"), ctx(10, "un", None, true));
        assert_eq!(scan_query("deps(//a) + rd"), ctx(12, "rd", None, false));
        assert_eq!(scan_query("deps(//a)+rd"), ctx(10, "rd", None, false));
        assert_eq!(scan_query("//a ^ "), ctx(6, "", None, false));
        assert_eq!(scan_query("//a - //b"), ctx(6, "//b", None, false));
        assert_eq!(scan_query("//a intersect "), ctx(14, "", None, false));
        assert_eq!(scan_query("//a union d"), ctx(10, "d", None, false));
        assert_eq!(
            scan_query("deps(//a intersect //b"),
            ctx(19, "//b", Some(("deps", 0)), false)
        );
        assert_eq!(scan_query("//a "), ctx(4, "", None, true));
        // Words in `set(...)` are separated by spaces.
        assert_eq!(
            scan_query("set(//a //b"),
            ctx(8, "//b", Some(("set", 0)), false)
        );
        assert_eq!(scan_query("set(//a "), ctx(8, "", Some(("set", 0)), false));
        assert_eq!(scan_query("(//a) "), ctx(6, "", None, true));
        assert_eq!(scan_query("(//a "), ctx(5, "", None, true));
    }

    #[test]
    fn test_quotes() {
        assert_eq!(
            scan_query("attrfilter(name, \"x y"),
            QueryContext {
                word_start: 18,
                word: "x y",
                quoted: true,
                call: Some(("attrfilter", 1)),
                operator: false,
            }
        );
        assert_eq!(
            scan_query("deps('//a'"),
            ctx(10, "", Some(("deps", 0)), true)
        );
        assert_eq!(
            scan_query("deps(\"//a(,\", "),
            ctx(14, "", Some(("deps", 1)), false)
        );
        assert_eq!(
            scan_query("deps('//a:b"),
            QueryContext {
                word_start: 6,
                word: "//a:b",
                quoted: true,
                call: Some(("deps", 0)),
                operator: false,
            }
        );
    }

    #[test]
    fn test_unbalanced() {
        assert_eq!(scan_query("))) d"), ctx(4, "d", None, true));
        assert_eq!(scan_query("a(b(c(d(e"), ctx(8, "e", Some(("d", 0)), false));
        assert_eq!(scan_query("é d"), ctx(3, "d", None, false));
        assert_eq!(
            scan_query("deps(%s"),
            ctx(5, "%s", Some(("deps", 0)), false)
        );
    }

    #[test]
    fn test_is_pattern_word() {
        for yes in ["//a", ":a", "a/b", "@c//", "...", "a.txt", "*"] {
            assert!(is_pattern_word(yes), "{yes}");
        }
        for no in ["", "deps", "first_order_deps", "a-b", "x1"] {
            assert!(!is_pattern_word(no), "{no}");
        }
    }

    #[test]
    fn test_query_args() {
        let args = [QueryArg::Targets, QueryArg::Integer, QueryArg::Expression];
        assert_eq!(encode_query_args(args), "targets,integer,expression");
        assert_eq!(
            decode_query_args("targets,integer,expression"),
            args.map(Some).to_vec()
        );
        assert_eq!(decode_query_args(""), vec![]);
        assert_eq!(
            decode_query_args("string,nope"),
            vec![Some(QueryArg::String), None]
        );
        for arg in QueryArg::ALL {
            assert_eq!(QueryArg::from_code(arg.code()), Some(arg));
        }
    }

    #[test]
    fn test_never_panics() {
        for s in [
            "",
            "(",
            ")",
            "\"",
            "'",
            ",",
            "é(",
            "a(\"é",
            "((((",
            "))))",
            "a b c(,,,",
        ] {
            for end in 0..=s.len() {
                if let Some(text) = s.get(..end) {
                    let _ignored = scan_query(text);
                }
            }
        }
    }
}
