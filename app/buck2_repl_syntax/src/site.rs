/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! What the cursor is on, for completion: the completion site.
//!
//! [`classify`] looks only at the text before the cursor, with the tolerant [`lexer`], so it
//! works on input that does not parse yet. It never runs or resolves anything: it says what
//! kind of word is being typed (a command name, a command argument, an identifier, an
//! argument of a call, an attribute at the end of a chain of attributes and calls, a target
//! pattern, a module or a symbol of a `load` in a string, or a word of a query), where the word
//! starts, and what comes before it. Where completion could only guess (after a subscript or a
//! literal, in a comment or an ordinary string, in a name being bound) it gives `None`.
//!
//! [`lexer`]: crate::lexer

use crate::commands::ArgKind;
use crate::commands::CommandId;
use crate::commands::QueryDialect;
use crate::commands::resolve_command;
use crate::commands::split_command_token;
use crate::lexer::Bracket;
use crate::lexer::Token;
use crate::lexer::TokenKind;
use crate::lexer::lex;
use crate::query::QueryContext;
use crate::query::scan_query;

/// Most steps in a chain of attributes and calls that completion follows.
pub const MAX_CHAIN_STEPS: usize = 64;

/// A step of a chain, between its root and the word being completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Step<'a> {
    /// `.name`
    Attr(&'a str),
    /// `(...)`: a call, whatever its arguments.
    Call,
}

/// What is being completed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SiteKind<'a> {
    /// A meta-command name. The prefix includes the colon (`:bu`).
    Command { prefix: &'a str },
    /// A word of a meta-command's argument: a target pattern (`:build`, `:run`, `:providers`),
    /// a module to load (`:load`), a BXL function (`:bxl`) or a help topic. (The words of a
    /// query are [`SiteKind::Query`], the symbols of `:load` [`SiteKind::LoadSymbol`].)
    CommandArg {
        command: CommandId,
        arg: ArgKind,
        word: &'a str,
    },
    /// An identifier (possibly empty).
    Name { prefix: &'a str },
    /// An attribute: `root.step.step.prefix`.
    Attr {
        root: &'a str,
        steps: Vec<Step<'a>>,
        prefix: &'a str,
    },
    /// An identifier (possibly empty) that starts an argument of a call, where a keyword
    /// argument may be typed too: `root.step.step(arg, name=arg, prefix`.
    CallArg {
        /// The function called: `root.step.step`.
        root: &'a str,
        steps: Vec<Step<'a>>,
        /// The keyword arguments given before the cursor.
        used: Vec<&'a str>,
        /// The number of positional arguments given before the cursor (not counting `*args`).
        positional: usize,
        prefix: &'a str,
    },
    /// The content of an unterminated string literal that looks like a target pattern.
    TargetString { prefix: &'a str },
    /// The content of the unterminated first string of a `load` statement: a module.
    LoadPath { prefix: &'a str },
    /// A symbol of `module` to load: the content of an unterminated later string of a `load`
    /// statement, or a later word of `:load`. `used` are the symbols loaded before it.
    LoadSymbol {
        module: &'a str,
        used: Vec<&'a str>,
        prefix: &'a str,
    },
    /// A word of a query: in the argument of `:uquery`, `:cquery` or `:aquery`, or in the
    /// unterminated query string of `ctx.cquery().eval("…` (and `uquery`, `aquery`).
    Query {
        dialect: QueryDialect,
        context: QueryContext<'a>,
    },
}

/// A completion site.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Site<'a> {
    /// Byte offset of the start of the word being completed: a candidate replaces the text from
    /// here to the cursor.
    pub start: usize,
    pub kind: SiteKind<'a>,
}

impl<'a> Site<'a> {
    /// The word before the cursor, which candidates start with.
    pub fn prefix(&self) -> &'a str {
        match &self.kind {
            SiteKind::Command { prefix }
            | SiteKind::Name { prefix }
            | SiteKind::Attr { prefix, .. }
            | SiteKind::CallArg { prefix, .. }
            | SiteKind::TargetString { prefix }
            | SiteKind::LoadPath { prefix }
            | SiteKind::LoadSymbol { prefix, .. } => prefix,
            SiteKind::CommandArg { word, .. } => word,
            SiteKind::Query { context, .. } => context.word,
        }
    }
}

/// Whether `s` looks like the start of a target pattern: `//x`, `:x`, `@cell//x` or `cell//x`.
pub fn looks_like_pattern(s: &str) -> bool {
    if s.contains(|c: char| c.is_whitespace()) {
        return false;
    }
    if s.starts_with("//") || s.starts_with(':') || s.starts_with('@') {
        return true;
    }
    match s.find("//") {
        Some(i) => s.get(..i).is_some_and(|cell| {
            cell.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        }),
        None => false,
    }
}

/// The completion site of the cursor at byte `pos` of `buf`. `None` when there is nothing to
/// complete there, or when `pos` is not a character boundary of `buf`.
pub fn classify(buf: &str, pos: usize) -> Option<Site<'_>> {
    let mut text = buf.get(..pos)?;
    // Byte offset of `text` in `buf`.
    let mut offset = 0;
    loop {
        let Some(token) = split_command_token(text) else {
            return starlark_site(text, offset);
        };
        let rest = text.get(token.arg_start..)?;
        if rest.is_empty() {
            // The cursor is at the end of the command token.
            let colon = token.token_start.checked_sub(1)?;
            return Some(Site {
                start: offset + colon,
                kind: SiteKind::Command {
                    prefix: text.get(colon..)?,
                },
            });
        }
        let spec = resolve_command(token.token).ok()?;
        match spec.arg {
            // Any input, possibly another command (`:time :b //x`).
            ArgKind::Input => {
                offset += token.arg_start;
                text = rest;
            }
            ArgKind::Expr => return starlark_site(rest, offset + token.arg_start),
            arg => return command_arg_site(spec.id, arg, rest, offset + token.arg_start),
        }
    }
}

/// The site in the argument `arg_text` of a command; the argument starts at byte `offset`.
fn command_arg_site(
    command: CommandId,
    arg: ArgKind,
    arg_text: &str,
    offset: usize,
) -> Option<Site<'_>> {
    if let ArgKind::Query(dialect) = arg {
        // A query is passed raw.
        let context = scan_query(arg_text);
        return Some(Site {
            start: offset + context.word_start,
            kind: SiteKind::Query { dialect, context },
        });
    }

    // Shell-like words: the last one is being typed.
    let start = arg_text
        .rfind(|c: char| c.is_ascii_whitespace())
        .map_or(0, |i| i + 1);
    let word = arg_text.get(start..)?;
    let before: Vec<&str> = arg_text.get(..start)?.split_ascii_whitespace().collect();
    if word.contains(['"', '\'', '\\']) {
        // Quoted or escaped words are not completed.
        return None;
    }
    if arg == ArgKind::Path
        && command == CommandId::Load
        && !word.starts_with('-')
        && let Some((module, used)) = before.split_first()
    {
        // `:load <module> <symbol>...`
        return Some(Site {
            start: offset + start,
            kind: SiteKind::LoadSymbol {
                module,
                used: used.to_vec(),
                prefix: word,
            },
        });
    }
    let completes = match arg {
        // Target patterns; flags are not completed.
        ArgKind::Targets => !word.starts_with('-'),
        // Only the first word: `:providers <target>`, `:load <module> [symbol...]`,
        // `:help <topic>`, `:bxl <file.bxl:function> [-- args...]`.
        ArgKind::Target | ArgKind::Topic | ArgKind::Path | ArgKind::BxlLabel => {
            before.is_empty() && !word.starts_with('-')
        }
        // `[--print] <target> [-- args...]`: the target, which is the first word that is not a
        // flag, before `--`.
        ArgKind::Run => {
            !word.starts_with('-')
                && !before.contains(&"--")
                && before.iter().all(|w| w.starts_with('-'))
        }
        _ => false,
    };
    completes.then_some(Site {
        start: offset + start,
        kind: SiteKind::CommandArg { command, arg, word },
    })
}

/// The site at the end of the Starlark code `text`, which starts at byte `offset`.
fn starlark_site(text: &str, offset: usize) -> Option<Site<'_>> {
    let tokens = lex(text);
    let Some(last_index) = tokens.len().checked_sub(1) else {
        // Nothing but whitespace.
        return Some(name(offset + text.len(), ""));
    };
    let last = tokens.get(last_index)?;
    let at_end = last.end == text.len();
    match last.kind {
        // Inside a comment.
        TokenKind::Comment => None,
        TokenKind::Str(info) => {
            let content = last.str_content()?;
            let letters = text.get(last.start..content.start)?;
            let prefix = text.get(content.clone())?;
            // An unterminated plain (or raw) string on one line.
            let plain = at_end
                && !info.closed
                && !info.continued
                && !info.quote.is_triple()
                && !letters.contains(['b', 'f'])
                && !prefix.contains('\\');
            if !plain {
                return None;
            }
            let start = offset + content.start;
            let kind = match string_argument(text, &tokens, last_index) {
                Some(StringArgument::LoadModule) => SiteKind::LoadPath { prefix },
                Some(StringArgument::LoadSymbol { module, used }) => SiteKind::LoadSymbol {
                    module,
                    used,
                    prefix,
                },
                Some(StringArgument::Pattern) if !prefix.contains(char::is_whitespace) => {
                    SiteKind::TargetString { prefix }
                }
                Some(StringArgument::Query(dialect)) => {
                    let context = scan_query(prefix);
                    return Some(Site {
                        start: start + context.word_start,
                        kind: SiteKind::Query { dialect, context },
                    });
                }
                Some(StringArgument::Pattern) | None if looks_like_pattern(prefix) => {
                    SiteKind::TargetString { prefix }
                }
                Some(StringArgument::Pattern) | None => return None,
            };
            Some(Site { start, kind })
        }
        TokenKind::Ident | TokenKind::Keyword if at_end => {
            let word = last.text(text);
            match last_index.checked_sub(1) {
                Some(previous) if tokens.get(previous)?.kind == TokenKind::Dot => {
                    attr_site(text, &tokens, previous, offset + last.start, word)
                }
                Some(previous) => name_may_follow(text, &tokens, previous).then(|| {
                    name_or_call_arg(text, &tokens, last_index, offset + last.start, word)
                }),
                None => Some(name(offset + last.start, word)),
            }
        }
        TokenKind::Dot => attr_site(text, &tokens, last_index, offset + text.len(), ""),
        _ => name_may_follow(text, &tokens, last_index)
            .then(|| name_or_call_arg(text, &tokens, tokens.len(), offset + text.len(), "")),
    }
}

fn name(start: usize, prefix: &str) -> Site<'_> {
    Site {
        start,
        kind: SiteKind::Name { prefix },
    }
}

/// Whether a token only lays out the code (inside brackets, it does not end a statement).
fn is_layout(token: &Token) -> bool {
    matches!(
        token.kind,
        TokenKind::Newline | TokenKind::Comment | TokenKind::Continuation
    )
}

/// The index of the last token before `tokens[end]` that is not layout.
fn previous_significant(tokens: &[Token], end: usize) -> Option<usize> {
    (0..end)
        .rev()
        .find(|&i| tokens.get(i).is_some_and(|t| !is_layout(t)))
}

/// The index of the innermost bracket that is still open just before `tokens[end]`.
fn innermost_open(tokens: &[Token], end: usize) -> Option<usize> {
    let mut depth = 0usize;
    for i in (0..end).rev() {
        match tokens.get(i)?.kind {
            TokenKind::Close(_) => depth += 1,
            TokenKind::Open(_) => match depth.checked_sub(1) {
                Some(d) => depth = d,
                None => return Some(i),
            },
            _ => {}
        }
    }
    None
}

/// The arguments of the call whose `(` is `tokens[open]`, before `tokens[end]` (the argument
/// being typed, which starts after the last comma): each one's significant tokens.
fn call_arguments(tokens: &[Token], open: usize, end: usize) -> Vec<Vec<usize>> {
    let mut arguments = vec![Vec::new()];
    let mut depth = 0usize;
    for i in open + 1..end {
        let Some(t) = tokens.get(i) else {
            break;
        };
        match t.kind {
            _ if is_layout(t) => continue,
            TokenKind::Comma if depth == 0 => {
                arguments.push(Vec::new());
                continue;
            }
            TokenKind::Open(_) => depth += 1,
            TokenKind::Close(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
        if let Some(argument) = arguments.last_mut() {
            argument.push(i);
        }
    }
    arguments
}

/// The keyword of the argument made of `argument` (token indices): `Some(name)` for
/// `name = ...`.
fn keyword_of<'a>(text: &'a str, tokens: &[Token], argument: &[usize]) -> Option<&'a str> {
    let name = tokens.get(*argument.first()?)?;
    let assign = tokens.get(*argument.get(1)?)?;
    (name.kind == TokenKind::Ident && assign.kind == TokenKind::Assign).then(|| name.text(text))
}

/// A name at the end of the code (the word `prefix`, possibly empty, which starts at byte
/// `start` and is `tokens[end]`, or follows the tokens if `end` is their number), where a name
/// may be: an argument of a call, if it starts one (`f(a, na`), else a name.
fn name_or_call_arg<'a>(
    text: &'a str,
    tokens: &[Token],
    end: usize,
    start: usize,
    prefix: &'a str,
) -> Site<'a> {
    call_arg(text, tokens, end, prefix)
        .map(|kind| Site { start, kind })
        .unwrap_or_else(|| name(start, prefix))
}

/// The call argument site of the word `tokens[end]` (or of an empty word after the tokens), if
/// it starts an argument of a call of a chain (`f(`, `ctx.cquery().deps(x, `).
fn call_arg<'a>(
    text: &'a str,
    tokens: &[Token],
    end: usize,
    prefix: &'a str,
) -> Option<SiteKind<'a>> {
    let open = innermost_open(tokens, end)?;
    if tokens.get(open)?.kind != TokenKind::Open(Bracket::Paren) {
        return None;
    }
    // The word starts an argument.
    let previous = previous_significant(tokens, end)?;
    if previous != open && tokens.get(previous)?.kind != TokenKind::Comma {
        return None;
    }
    // A call: the parenthesis follows the end of a chain.
    let (root, steps) = chain(text, tokens, open)?;
    let arguments = call_arguments(tokens, open, end);
    let mut used = Vec::new();
    let mut positional = 0usize;
    // The last argument is the one being typed.
    for argument in arguments.iter().take(arguments.len().saturating_sub(1)) {
        let Some(first) = argument.first().and_then(|&i| tokens.get(i)) else {
            continue;
        };
        if let Some(keyword) = keyword_of(text, tokens, argument) {
            used.push(keyword);
        } else if !(first.is_op(text, "*") || first.is_op(text, "**")) {
            positional += 1;
        }
    }
    Some(SiteKind::CallArg {
        root,
        steps,
        used,
        positional,
        prefix,
    })
}

/// What an unterminated string that is an argument of a call is.
enum StringArgument<'a> {
    /// The module of a `load`.
    LoadModule,
    /// A symbol of a `load`.
    LoadSymbol {
        module: &'a str,
        /// The other symbols loaded.
        used: Vec<&'a str>,
    },
    /// The query of `ctx.cquery().eval(...)` (or `uquery`, `aquery`).
    Query(QueryDialect),
    /// The targets of a method of the BXL context that takes target patterns first
    /// (`ctx.configured_targets(...)`): a pattern even if it does not look like one yet
    /// (`"<TAB>`, `"lib:<TAB>`).
    Pattern,
}

/// The query contexts' method that evaluates a query, and its parameter.
const QUERY_EVAL: (&str, &str) = ("eval", "query");

/// The methods of the BXL context whose first parameter takes target patterns, and the names of
/// that parameter.
const PATTERN_METHODS: &[&str] = &[
    "analysis",
    "build",
    "configured_targets",
    "target_exists",
    "target_universe",
    "unconfigured_sub_targets",
    "unconfigured_targets",
];
const PATTERN_PARAMETERS: &[&str] = &["label", "labels"];

/// What the string `tokens[string]` (the last token) is an argument of, if it is a whole
/// argument of a `load`, or the query of a query context's `eval`.
fn string_argument<'a>(
    text: &'a str,
    tokens: &[Token],
    string: usize,
) -> Option<StringArgument<'a>> {
    let open = innermost_open(tokens, string)?;
    if tokens.get(open)?.kind != TokenKind::Open(Bracket::Paren) {
        return None;
    }
    let arguments = call_arguments(tokens, open, string);
    let (current, before) = arguments.split_last()?;
    let keyword = match current.as_slice() {
        [] => None,
        [name, assign]
            if tokens.get(*name)?.kind == TokenKind::Ident
                && tokens.get(*assign)?.kind == TokenKind::Assign =>
        {
            Some(tokens.get(*name)?.text(text))
        }
        _ => return None,
    };
    let callee = previous_significant(tokens, open).and_then(|i| tokens.get(i))?;
    if callee.is_keyword(text, "load") {
        if before.is_empty() && keyword.is_none() {
            return Some(StringArgument::LoadModule);
        }
        // The module is the first argument, a closed string.
        let (first, others) = before.split_first()?;
        let module = closed_string(text, tokens, first)?;
        let used = others
            .iter()
            .filter_map(|argument| {
                // `"symbol"` or `local = "symbol"`.
                let value = match keyword_of(text, tokens, argument) {
                    Some(_) => argument.get(2..)?,
                    None => argument.as_slice(),
                };
                closed_string(text, tokens, value)
            })
            .collect();
        return Some(StringArgument::LoadSymbol { module, used });
    }
    let (_, steps) = chain(text, tokens, open)?;
    // The first argument, or the one named `query`.
    let query = match keyword {
        None => before.is_empty(),
        Some(keyword) => keyword == QUERY_EVAL.1,
    };
    let first = match keyword {
        None => before.is_empty(),
        Some(keyword) => PATTERN_PARAMETERS.contains(&keyword),
    };
    match steps.as_slice() {
        [.., Step::Attr(method)] if first && PATTERN_METHODS.contains(method) => {
            Some(StringArgument::Pattern)
        }
        [.., Step::Attr(method), Step::Call, Step::Attr(eval)]
            if query && *eval == QUERY_EVAL.0 =>
        {
            let dialect = [
                QueryDialect::Uquery,
                QueryDialect::Cquery,
                QueryDialect::Aquery,
            ]
            .into_iter()
            .find(|d| d.ctx_method() == *method)?;
            Some(StringArgument::Query(dialect))
        }
        _ => None,
    }
}

/// The content of the argument made of `argument` (token indices), if it is one closed string
/// without escapes or prefix letters.
fn closed_string<'a>(text: &'a str, tokens: &[Token], argument: &[usize]) -> Option<&'a str> {
    let [i] = argument else {
        return None;
    };
    let token = tokens.get(*i)?;
    let TokenKind::Str(info) = token.kind else {
        return None;
    };
    let content = text.get(token.str_content()?)?;
    (info.closed && !info.continued && info.prefix_len == 0 && !content.contains('\\'))
        .then_some(content)
}

/// The attribute `prefix`, which starts at byte `start` and follows the dot `tokens[dot]`.
fn attr_site<'a>(
    text: &'a str,
    tokens: &[Token],
    dot: usize,
    start: usize,
    prefix: &'a str,
) -> Option<Site<'a>> {
    let (root, steps) = chain(text, tokens, dot)?;
    Some(Site {
        start,
        kind: SiteKind::Attr {
            root,
            steps,
            prefix,
        },
    })
}

/// The chain of attributes and calls that ends just before `tokens[end]`: its root identifier
/// and its steps. `None` if it does not start with an identifier (a literal, a subscript, ...)
/// or is too long.
fn chain<'a>(text: &'a str, tokens: &[Token], end: usize) -> Option<(&'a str, Vec<Step<'a>>)> {
    let mut steps = Vec::new();
    let mut end = end;
    loop {
        if steps.len() > MAX_CHAIN_STEPS {
            return None;
        }
        let i = end.checked_sub(1)?;
        let t = tokens.get(i)?;
        match t.kind {
            TokenKind::Ident => {
                let name = t.text(text);
                match i.checked_sub(1).map(|j| (j, tokens.get(j))) {
                    Some((
                        dot,
                        Some(Token {
                            kind: TokenKind::Dot,
                            ..
                        }),
                    )) => {
                        steps.push(Step::Attr(name));
                        end = dot;
                    }
                    _ => {
                        steps.reverse();
                        return Some((name, steps));
                    }
                }
            }
            TokenKind::Close(Bracket::Paren) => {
                steps.push(Step::Call);
                end = matching_open(tokens, i)?;
            }
            _ => return None,
        }
    }
}

/// The index of the opening bracket that matches the closing one at `close`.
fn matching_open(tokens: &[Token], close: usize) -> Option<usize> {
    let mut depth = 0usize;
    for i in (0..=close).rev() {
        match tokens.get(i)?.kind {
            TokenKind::Close(_) => depth += 1,
            TokenKind::Open(bracket) => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return (bracket == Bracket::Paren).then_some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Keywords that an expression, so a name, may follow.
const EXPRESSION_KEYWORDS: &[&str] = &["and", "elif", "else", "if", "in", "not", "or", "return"];

/// Keywords that names being bound follow.
const BINDING_KEYWORDS: &[&str] = &["def", "for", "lambda"];

/// Whether a name may follow `tokens[i]` (the token before it) as a use of the name: not after a
/// value (an identifier, a literal, a closing bracket), and not where a name is bound (after
/// `def`, `for` or `lambda`, or in a parameter list).
fn name_may_follow(text: &str, tokens: &[Token], i: usize) -> bool {
    let Some(t) = tokens.get(i) else {
        return true;
    };
    match t.kind {
        TokenKind::Ident
        | TokenKind::Int
        | TokenKind::Float
        | TokenKind::Str(_)
        | TokenKind::Close(_)
        | TokenKind::Comment
        | TokenKind::Continuation
        | TokenKind::Error => false,
        TokenKind::Keyword => EXPRESSION_KEYWORDS.contains(&t.text(text)),
        TokenKind::Comma | TokenKind::Open(_) => !binds_after(text, tokens, i),
        TokenKind::Op if matches!(t.text(text), "*" | "**") => {
            // `*args` in a parameter list, or a star argument of a call.
            let Some((j, before)) = i.checked_sub(1).and_then(|j| Some((j, tokens.get(j)?))) else {
                return true;
            };
            match before.kind {
                TokenKind::Keyword => !BINDING_KEYWORDS.contains(&before.text(text)),
                TokenKind::Comma | TokenKind::Open(_) => !binds_after(text, tokens, j),
                _ => true,
            }
        }
        TokenKind::Dot
        | TokenKind::Colon
        | TokenKind::Semicolon
        | TokenKind::Assign
        | TokenKind::Arrow
        | TokenKind::Op
        | TokenKind::Newline => true,
    }
}

/// Whether a name after `tokens[i]`, a comma or an opening bracket, is being bound: a parameter
/// of a `def` or a `lambda`, or a target of a `for`.
fn binds_after(text: &str, tokens: &[Token], i: usize) -> bool {
    let mut depth = 0usize;
    let mut j = i;
    loop {
        let Some(t) = tokens.get(j) else {
            return false;
        };
        match t.kind {
            TokenKind::Close(_) => depth += 1,
            TokenKind::Open(bracket) => {
                if depth == 0 {
                    // The bracket around the name: the parameters of a `def`?
                    let before = |n: usize| j.checked_sub(n).and_then(|k| tokens.get(k));
                    return bracket == Bracket::Paren
                        && before(1).is_some_and(|t| t.kind == TokenKind::Ident)
                        && before(2).is_some_and(|t| t.is_keyword(text, "def"));
                }
                depth -= 1;
            }
            TokenKind::Keyword if depth == 0 => match t.text(text) {
                "lambda" | "for" => return true,
                "in" | "return" | "def" | "load" => return false,
                // Inside default values (`lambda a = x if c else y, b`).
                _ => {}
            },
            TokenKind::Newline | TokenKind::Colon | TokenKind::Semicolon if depth == 0 => {
                return false;
            }
            _ => {}
        }
        match j.checked_sub(1) {
            Some(previous) => j = previous,
            None => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Classifies `input`, where `▮` marks the cursor.
    fn site(input: &str) -> Option<Site<'static>> {
        let pos = input.find('▮').expect("no cursor");
        let buf: &'static str = Box::leak(input.replacen('▮', "", 1).into_boxed_str());
        classify(buf, pos)
    }

    fn name_at(start: usize, prefix: &'static str) -> Option<Site<'static>> {
        Some(Site {
            start,
            kind: SiteKind::Name { prefix },
        })
    }

    fn attr_at(
        start: usize,
        root: &'static str,
        steps: Vec<Step<'static>>,
        prefix: &'static str,
    ) -> Option<Site<'static>> {
        Some(Site {
            start,
            kind: SiteKind::Attr {
                root,
                steps,
                prefix,
            },
        })
    }

    fn arg_at(
        start: usize,
        command: CommandId,
        arg: ArgKind,
        word: &'static str,
    ) -> Option<Site<'static>> {
        Some(Site {
            start,
            kind: SiteKind::CommandArg { command, arg, word },
        })
    }

    fn target_string_at(start: usize, prefix: &'static str) -> Option<Site<'static>> {
        Some(Site {
            start,
            kind: SiteKind::TargetString { prefix },
        })
    }

    fn call_arg_at(
        start: usize,
        root: &'static str,
        steps: Vec<Step<'static>>,
        used: Vec<&'static str>,
        positional: usize,
        prefix: &'static str,
    ) -> Option<Site<'static>> {
        Some(Site {
            start,
            kind: SiteKind::CallArg {
                root,
                steps,
                used,
                positional,
                prefix,
            },
        })
    }

    fn load_path_at(start: usize, prefix: &'static str) -> Option<Site<'static>> {
        Some(Site {
            start,
            kind: SiteKind::LoadPath { prefix },
        })
    }

    fn load_symbol_at(
        start: usize,
        module: &'static str,
        used: Vec<&'static str>,
        prefix: &'static str,
    ) -> Option<Site<'static>> {
        Some(Site {
            start,
            kind: SiteKind::LoadSymbol {
                module,
                used,
                prefix,
            },
        })
    }

    /// A query site whose word starts at `start` in the input and at `word_start` in the query.
    fn query_at(
        start: usize,
        dialect: QueryDialect,
        word_start: usize,
        word: &'static str,
        call: Option<(&'static str, usize)>,
        operator: bool,
    ) -> Option<Site<'static>> {
        Some(Site {
            start,
            kind: SiteKind::Query {
                dialect,
                context: QueryContext {
                    word_start,
                    word,
                    quoted: false,
                    call,
                    operator,
                },
            },
        })
    }

    // The cases of the spec (§6), in order.

    #[test]
    fn test_01_colon() {
        assert_eq!(
            site(":▮"),
            Some(Site {
                start: 0,
                kind: SiteKind::Command { prefix: ":" }
            })
        );
    }

    #[test]
    fn test_02_command_prefix() {
        assert_eq!(
            site(":bu▮"),
            Some(Site {
                start: 0,
                kind: SiteKind::Command { prefix: ":bu" }
            })
        );
    }

    #[test]
    fn test_03_build_target() {
        assert_eq!(
            site(":b //fo▮"),
            arg_at(3, CommandId::Build, ArgKind::Targets, "//fo")
        );
    }

    #[test]
    fn test_04_run_program_args() {
        assert_eq!(site(":run //:greet -- --x▮"), None);
    }

    #[test]
    fn test_05_load_path() {
        assert_eq!(
            site(":l hel▮"),
            arg_at(3, CommandId::Load, ArgKind::Path, "hel")
        );
    }

    #[test]
    fn test_06_type_argument_is_starlark() {
        // The start is an offset in the whole input.
        assert_eq!(site(":t ctx.o▮"), attr_at(7, "ctx", vec![], "o"));
    }

    #[test]
    fn test_07_name() {
        assert_eq!(site("ct▮"), name_at(0, "ct"));
    }

    #[test]
    fn test_08_name_after_assign() {
        assert_eq!(site("x = ct▮"), name_at(4, "ct"));
    }

    #[test]
    fn test_09_attr_empty_prefix() {
        assert_eq!(site("ctx.▮"), attr_at(4, "ctx", vec![], ""));
    }

    #[test]
    fn test_10_attr_chain() {
        assert_eq!(
            site("ctx.output.pr▮"),
            attr_at(11, "ctx", vec![Step::Attr("output")], "pr")
        );
    }

    #[test]
    fn test_11_attr_through_call() {
        assert_eq!(
            site("ctx.cquery().de▮"),
            attr_at(13, "ctx", vec![Step::Attr("cquery"), Step::Call], "de")
        );
    }

    #[test]
    fn test_12_attr_through_call_with_arguments() {
        let input = "ctx.cquery(\"a\", f(b)).de▮";
        assert_eq!(
            site(input),
            attr_at(
                input.find("de").unwrap(),
                "ctx",
                vec![Step::Attr("cquery"), Step::Call],
                "de"
            )
        );
    }

    #[test]
    fn test_13_attr_in_call() {
        assert_eq!(site("f(ctx.an▮"), attr_at(6, "ctx", vec![], "an"));
    }

    #[test]
    fn test_14_attr_in_block() {
        assert_eq!(
            site("def f():\n    ctx.cq▮"),
            attr_at(17, "ctx", vec![], "cq")
        );
    }

    #[test]
    fn test_15_after_subscript() {
        assert_eq!(site("x[0].▮"), None);
    }

    #[test]
    fn test_16_after_literal() {
        assert_eq!(site("\"abc\".up▮"), None);
    }

    #[test]
    fn test_17_comment() {
        assert_eq!(site("# ctx.▮"), None);
    }

    #[test]
    fn test_18_binding_positions() {
        assert_eq!(site("def fo▮"), None);
        assert_eq!(site("for t▮"), None);
        assert_eq!(site("lambda a▮"), None);
    }

    #[test]
    fn test_19_target_string() {
        assert_eq!(
            site("ctx.configured_targets(\"//pk▮"),
            target_string_at(24, "//pk")
        );
    }

    #[test]
    fn test_20_relative_target_string() {
        assert_eq!(site("ctx.analysis(\":gr▮"), target_string_at(14, ":gr"));
    }

    #[test]
    fn test_21_plain_string() {
        assert_eq!(site("print(\"hello wo▮"), None);
    }

    #[test]
    fn test_22_float() {
        assert_eq!(site("1.▮"), None);
    }

    #[test]
    fn test_23_after_call() {
        assert_eq!(site("ctx.cquery()▮"), None);
    }

    // More cases.

    #[test]
    fn test_empty_and_whitespace() {
        assert_eq!(site("▮"), name_at(0, ""));
        assert_eq!(site("x = ▮"), name_at(4, ""));
        assert_eq!(site("f(▮"), call_arg_at(2, "f", vec![], vec![], 0, ""));
        assert_eq!(site("f(a, ▮"), call_arg_at(5, "f", vec![], vec![], 1, ""));
        assert_eq!(site("if x and ▮"), name_at(9, ""));
        assert_eq!(site("x ▮"), None);
        assert_eq!(site("f() ▮"), None);
        assert_eq!(site("pass ▮"), None);
    }

    #[test]
    fn test_cursor_in_the_middle() {
        // Only the text before the cursor matters.
        assert_eq!(site("ct▮x.foo"), name_at(0, "ct"));
        assert_eq!(
            site(":b //a▮ //b"),
            arg_at(3, CommandId::Build, ArgKind::Targets, "//a")
        );
    }

    #[test]
    fn test_bad_positions() {
        assert_eq!(classify("abc", 4), None);
        assert_eq!(classify("é", 1), None);
    }

    #[test]
    fn test_keywords_as_prefix() {
        assert_eq!(site("x = no▮"), name_at(4, "no"));
        assert_eq!(site("x = not▮"), name_at(4, "not"));
        assert_eq!(site("return x▮"), name_at(7, "x"));
        assert_eq!(site("x if c else d▮"), name_at(12, "d"));
    }

    #[test]
    fn test_more_binding_positions() {
        assert_eq!(site("def f(a, b▮"), None);
        assert_eq!(site("def f(a▮"), None);
        assert_eq!(site("def f(a, *ar▮"), None);
        assert_eq!(site("def f(a = 1, b▮"), None);
        assert_eq!(site("lambda a, b▮"), None);
        assert_eq!(site("[x for x, y▮"), None);
        assert_eq!(site("f(lambda a, b▮"), None);
        // But not default values and other uses.
        assert_eq!(site("def f(a = b▮"), name_at(10, "b"));
        assert_eq!(
            site("def f(a = g(b▮"),
            call_arg_at(12, "g", vec![], vec![], 0, "b")
        );
        assert_eq!(site("lambda a: b▮"), name_at(10, "b"));
        assert_eq!(site("for x in y▮"), name_at(9, "y"));
        assert_eq!(site("for x in a, b▮"), name_at(12, "b"));
        assert_eq!(site("f(a, b▮"), call_arg_at(5, "f", vec![], vec![], 1, "b"));
        assert_eq!(site("f(a, *b▮"), name_at(6, "b"));
        assert_eq!(site("x = a * b▮"), name_at(8, "b"));
        assert_eq!(site("x = a, b▮"), name_at(7, "b"));
    }

    #[test]
    fn test_more_chains() {
        assert_eq!(
            site("a.b.c.d▮"),
            attr_at(6, "a", vec![Step::Attr("b"), Step::Attr("c")], "d")
        );
        assert_eq!(
            site("f()().x▮"),
            attr_at(6, "f", vec![Step::Call, Step::Call], "x")
        );
        assert_eq!(site("x = f(ctx.cquery(\")\").eval(\"//...\")[0].de▮"), None);
        assert_eq!(
            site("f(\n  ctx.cquery(\n  ).de▮"),
            attr_at(21, "ctx", vec![Step::Attr("cquery"), Step::Call], "de")
        );
        assert_eq!(site("(a).b▮"), None);
        assert_eq!(site("f[0](x).b▮"), None);
        assert_eq!(site("{}.ke▮"), None);
        assert_eq!(site(".x▮"), None);
        assert_eq!(site("x)).y▮"), None);
        // Too long a chain.
        let long = format!("x{}.y▮", ".a".repeat(MAX_CHAIN_STEPS + 5));
        assert_eq!(site(&long), None);
    }

    #[test]
    fn test_strings() {
        assert_eq!(site("x = \"//a/b:c▮"), target_string_at(5, "//a/b:c"));
        assert_eq!(site("x = 'cell//a▮"), target_string_at(5, "cell//a"));
        assert_eq!(site("x = r\"@c//a▮"), target_string_at(6, "@c//a"));
        assert_eq!(site("x = \"▮"), None);
        assert_eq!(site("x = \"//a\"▮"), None);
        assert_eq!(site("x = \"\"\"//a▮"), None);
        assert_eq!(site("x = f\"//a▮"), None);
        assert_eq!(site("x = b\"//a▮"), None);
        assert_eq!(site("x = \"//a\\n▮"), None);
        assert_eq!(site("x = \"a//b c//d▮"), None);
    }

    #[test]
    fn test_commands() {
        assert_eq!(
            site("  :▮"),
            Some(Site {
                start: 2,
                kind: SiteKind::Command { prefix: ":" }
            })
        );
        assert_eq!(
            site(":?▮"),
            Some(Site {
                start: 0,
                kind: SiteKind::Command { prefix: ":?" }
            })
        );
        // Unknown or ambiguous command with an argument.
        assert_eq!(site(":zz //a▮"), None);
        assert_eq!(site(":re //a▮"), None);
        assert_eq!(
            site(":b ▮"),
            arg_at(3, CommandId::Build, ArgKind::Targets, "")
        );
        assert_eq!(
            site(":build //a:b //c/d▮"),
            arg_at(13, CommandId::Build, ArgKind::Targets, "//c/d")
        );
        assert_eq!(site(":b -▮"), None);
        assert_eq!(site(":b \"//a▮"), None);
        assert_eq!(site(":run ▮"), arg_at(5, CommandId::Run, ArgKind::Run, ""));
        assert_eq!(
            site(":bxl pkg/x.bxl:ma▮"),
            arg_at(5, CommandId::Bxl, ArgKind::BxlLabel, "pkg/x.bxl:ma")
        );
        assert_eq!(
            site(":bxl //pkg:x▮"),
            arg_at(5, CommandId::Bxl, ArgKind::BxlLabel, "//pkg:x")
        );
        assert_eq!(site(":bxl x.bxl:main --▮"), None);
        assert_eq!(site(":bxl x.bxl:main -- --na▮"), None);
        assert_eq!(site(":bxl x.bxl:main -- a▮"), None);
        assert_eq!(
            site(":run --print //:gr▮"),
            arg_at(13, CommandId::Run, ArgKind::Run, "//:gr")
        );
        assert_eq!(site(":run //:greet x▮"), None);
        assert_eq!(site(":run --pr▮"), None);
        assert_eq!(
            site(":pv :he▮"),
            arg_at(4, CommandId::Providers, ArgKind::Target, ":he")
        );
        assert_eq!(site(":pv //a //b▮"), None);
        assert_eq!(
            site(":l hel.bzl sym▮"),
            load_symbol_at(11, "hel.bzl", vec![], "sym")
        );
        assert_eq!(
            site(":load //a:b.bzl x y▮"),
            load_symbol_at(18, "//a:b.bzl", vec!["x"], "y")
        );
        assert_eq!(site(":load //a:b.bzl -▮"), None);
        assert_eq!(
            site(":help bu▮"),
            arg_at(6, CommandId::Help, ArgKind::Topic, "bu")
        );
        assert_eq!(site(":q ▮"), None);
        assert_eq!(site(":reset ▮"), None);
        assert_eq!(site(":__complete {▮"), None);
    }

    #[test]
    fn test_nested_inputs() {
        assert_eq!(
            site(":time :b //fo▮"),
            arg_at(9, CommandId::Build, ArgKind::Targets, "//fo")
        );
        assert_eq!(
            site(":time :ti▮"),
            Some(Site {
                start: 6,
                kind: SiteKind::Command { prefix: ":ti" }
            })
        );
        assert_eq!(site(":time ct▮"), name_at(6, "ct"));
        assert_eq!(site(":time :time ctx.x▮"), attr_at(16, "ctx", vec![], "x"));
        assert_eq!(site(":p \"//a▮"), target_string_at(4, "//a"));
    }

    #[test]
    fn test_queries() {
        assert_eq!(
            site(":cq deps(//fo▮"),
            query_at(9, QueryDialect::Cquery, 6, "//fo", Some(("deps", 0)), false)
        );
        assert_eq!(
            site(":uq :li▮"),
            query_at(4, QueryDialect::Uquery, 1, ":li", None, false)
        );
        assert_eq!(
            site(":cq dep▮"),
            query_at(4, QueryDialect::Cquery, 1, "dep", None, false)
        );
        assert_eq!(
            site(":cq ▮"),
            query_at(4, QueryDialect::Cquery, 1, "", None, false)
        );
        assert_eq!(
            site(":cq deps(\"//a\", 1▮"),
            query_at(16, QueryDialect::Cquery, 13, "1", Some(("deps", 1)), false)
        );
        assert_eq!(
            site(":aquery deps(//a) un▮"),
            query_at(18, QueryDialect::Aquery, 11, "un", None, true)
        );
        assert_eq!(
            site(":time :cq de▮"),
            query_at(10, QueryDialect::Cquery, 1, "de", None, false)
        );
    }

    #[test]
    fn test_query_strings() {
        let input = "ctx.cquery().eval(\"rde▮";
        assert_eq!(
            site(input),
            query_at(19, QueryDialect::Cquery, 0, "rde", None, false)
        );
        assert_eq!(
            site("ctx.uquery().eval(\"deps(//li▮"),
            query_at(
                24,
                QueryDialect::Uquery,
                5,
                "//li",
                Some(("deps", 0)),
                false
            )
        );
        assert_eq!(
            site("x = ctx.aquery(a, b).eval(query = 'all_▮"),
            query_at(35, QueryDialect::Aquery, 0, "all_", None, false)
        );
        assert_eq!(
            site("ctx.cquery().eval(\n    \"deps(▮"),
            query_at(29, QueryDialect::Cquery, 5, "", Some(("deps", 0)), false)
        );
        // A pattern in a query.
        assert_eq!(
            site("ctx.cquery().eval(\"//li▮"),
            query_at(19, QueryDialect::Cquery, 0, "//li", None, false)
        );
        // Not the query: other arguments are strings like others.
        assert_eq!(
            site("ctx.cquery().eval(\"%s\", query_args = [\"//a▮"),
            target_string_at(39, "//a")
        );
        assert_eq!(
            site("ctx.cquery().eval(\"x\", \"//a▮"),
            target_string_at(24, "//a")
        );
        assert_eq!(site("ctx.cquery().eval(\"x\", \"de▮"), None);
        assert_eq!(site("ctx.cquery().eval(\"a\" + \"de▮"), None);
        assert_eq!(site("ctx.cquery().eval(target_universe = \"de▮"), None);
        // The receiver is not known to be a query context.
        assert_eq!(site("q.eval(\"rde▮"), None);
        assert_eq!(site("ctx.cquery.eval(\"rde▮"), None);
        assert_eq!(site("ctx.bquery().eval(\"rde▮"), None);
        // Escapes, f-strings and triple quotes are not completed.
        assert_eq!(site("ctx.cquery().eval(\"a\\\"b▮"), None);
        assert_eq!(site("ctx.cquery().eval(f\"rde▮"), None);
        assert_eq!(site("ctx.cquery().eval(\"\"\"rde▮"), None);
    }

    #[test]
    fn test_call_arguments() {
        let input = "ctx.configured_targets(tar▮";
        assert_eq!(
            site(input),
            call_arg_at(
                23,
                "ctx",
                vec![Step::Attr("configured_targets")],
                vec![],
                0,
                "tar"
            )
        );
        assert_eq!(
            site("ctx.configured_targets(\"//x\", ▮"),
            call_arg_at(
                30,
                "ctx",
                vec![Step::Attr("configured_targets")],
                vec![],
                1,
                ""
            )
        );
        assert_eq!(
            site("f(a = 1, *args, b, **kw, c▮"),
            call_arg_at(25, "f", vec![], vec!["a"], 1, "c")
        );
        assert_eq!(
            site("f(\n    x,  # the first\n    ▮"),
            call_arg_at(27, "f", vec![], vec![], 1, "")
        );
        assert_eq!(
            site("ctx.cquery().deps(x, depth = 1, fi▮"),
            call_arg_at(
                32,
                "ctx",
                vec![Step::Attr("cquery"), Step::Call, Step::Attr("deps")],
                vec!["depth"],
                1,
                "fi"
            )
        );
        assert_eq!(
            site("f(a = [1, 2], g(x, y = 3), ▮"),
            call_arg_at(27, "f", vec![], vec!["a"], 1, "")
        );
        assert_eq!(
            site("f(x)(▮"),
            call_arg_at(5, "f", vec![Step::Call], vec![], 0, "")
        );
        assert_eq!(site("print(x if c else d▮"), name_at(18, "d"));
        // Not calls.
        assert_eq!(site("x = (a, ▮"), name_at(8, ""));
        assert_eq!(site("[f(x), ▮"), name_at(7, ""));
        assert_eq!(site("load(▮"), name_at(5, ""));
        assert_eq!(site("x[0](▮"), name_at(5, ""));
        assert_eq!(site("f(a = ▮"), name_at(6, ""));
        assert_eq!(site("f(a = b▮"), name_at(6, "b"));
        assert_eq!(site("f(*▮"), name_at(3, ""));
        assert_eq!(site("f(a ▮"), None);
    }

    #[test]
    fn test_pattern_arguments() {
        assert_eq!(site("ctx.configured_targets(\"▮"), target_string_at(24, ""));
        assert_eq!(site("ctx.analysis(\"lib:a▮"), target_string_at(14, "lib:a"));
        assert_eq!(
            site("x.unconfigured_targets(labels = \"li▮"),
            target_string_at(33, "li")
        );
        // Other arguments: only strings that look like patterns.
        assert_eq!(site("ctx.configured_targets(x, \"li▮"), None);
        assert_eq!(
            site("ctx.configured_targets(x, target_platform = \"//p▮"),
            target_string_at(45, "//p")
        );
        assert_eq!(site("ctx.configured_targets(\"a b▮"), None);
        assert_eq!(site("ctx.output.print(\"li▮"), None);
    }

    #[test]
    fn test_loads() {
        assert_eq!(site("load(\"//pk▮"), load_path_at(6, "//pk"));
        assert_eq!(site("load(':he▮"), load_path_at(6, ":he"));
        assert_eq!(site("load(\"▮"), load_path_at(6, ""));
        assert_eq!(site("load(r\"//x:y▮"), load_path_at(7, "//x:y"));
        assert_eq!(
            site("load(\"//x:y.bzl\", \"dou▮"),
            load_symbol_at(19, "//x:y.bzl", vec![], "dou")
        );
        assert_eq!(
            site("load(\"//x:y.bzl\", \"a\", b = \"c\", \"▮"),
            load_symbol_at(33, "//x:y.bzl", vec!["a", "c"], "")
        );
        assert_eq!(
            site("load(\"a.bzl\", x = \"sy▮"),
            load_symbol_at(19, "a.bzl", vec![], "sy")
        );
        assert_eq!(
            site("load(\n    \"//x:y.bzl\",\n    \"dou▮"),
            load_symbol_at(28, "//x:y.bzl", vec![], "dou")
        );
        // The module is not a plain string.
        assert_eq!(site("load(m, \"▮"), None);
        assert_eq!(site("load(f\"m\", \"▮"), None);
        assert_eq!(site("load(\"//x:y.bzl\", \"a\" + \"▮"), None);
        assert_eq!(site("load(\"//x:y.bzl\" \"▮"), None);
        // Not a load.
        assert_eq!(site("loads(\"//x▮"), target_string_at(7, "//x"));
    }

    #[test]
    fn test_looks_like_pattern() {
        for yes in ["//", "//a", ":", ":x", "@c//", "c//x", "my-cell//"] {
            assert!(looks_like_pattern(yes), "{yes}");
        }
        for no in ["", "a", "a/b", "a b//", "x.y//", "/abs"] {
            assert!(!looks_like_pattern(no), "{no}");
        }
    }

    #[test]
    fn test_prefix() {
        assert_eq!(site("ctx.cq▮").unwrap().prefix(), "cq");
        assert_eq!(site(":b //a▮").unwrap().prefix(), "//a");
    }

    #[test]
    fn test_never_panics() {
        let samples = [
            "",
            ":",
            "::",
            ": ",
            ":\\",
            "(",
            ")",
            ").",
            "(.",
            "\"",
            "'",
            "\"\"\"",
            "#",
            ".",
            "..",
            "a..",
            "a.(",
            "f(.",
            "def",
            "def(",
            "lambda",
            "lambda,",
            "for,",
            "*",
            "**",
            ",",
            "é",
            "é.",
            "x.é",
            ":bé",
            ":b é",
            ":t",
            ":t ",
            ":time",
            ":time :",
            ":time :time",
            ":cq (",
            ":run --",
            "\\",
            "x = \\",
            "a,",
            "(,",
            "[x for",
            "f(*",
            "(*",
            "*,",
            "load(",
            "load(\"",
            "load(\"a\", \"",
            "load(\"a\", x = \"",
            "load(\"a\" , , \"",
            "load(,\"",
            "load(\"a\"\"",
            ")(\"",
            "f(a=",
            "f(a=,",
            "f(=",
            "f(,,,",
            "(=\"",
            "x.eval(\"",
            "ctx.cquery().eval(\"deps(\"",
            "ctx.cquery().eval(query=\"",
            "ctx.cquery().eval(=\"",
            ".eval(\"",
            "().eval(\"",
            ":cq \"",
            ":cq (((",
            ":cq )))",
            ":l a b",
            ":l a \"",
        ];
        for s in samples {
            for pos in 0..=s.len() + 1 {
                let _ignored = classify(s, pos);
            }
        }
    }
}
