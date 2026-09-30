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
//! attribute at the end of a chain of attributes and calls, or a target pattern in a string),
//! where the word starts, and what comes before it. Where completion could only guess (after a
//! subscript or a literal, in a comment or an ordinary string, in a name being bound) it gives
//! `None`.
//!
//! [`lexer`]: crate::lexer

use crate::commands::ArgKind;
use crate::commands::CommandId;
use crate::commands::resolve_command;
use crate::commands::split_command_token;
use crate::lexer::Bracket;
use crate::lexer::Token;
use crate::lexer::TokenKind;
use crate::lexer::lex;

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
    /// A word of a meta-command's argument: a target pattern (`:build`, `:run`, `:providers`,
    /// or a pattern in a query), a path (`:load`), a BXL function (`:bxl`) or a help topic.
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
    /// The content of an unterminated string literal that looks like a target pattern.
    TargetString { prefix: &'a str },
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
            | SiteKind::TargetString { prefix } => prefix,
            SiteKind::CommandArg { word, .. } => word,
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
    if let ArgKind::Query(_) = arg {
        // A query is passed raw: a word ends at a space, a bracket, a comma or a quote. Only
        // target patterns are completed in it.
        let start = arg_text
            .rfind(|c: char| c.is_ascii_whitespace() || "(),\"'".contains(c))
            .map_or(0, |i| i + 1);
        let word = arg_text.get(start..)?;
        return looks_like_pattern(word).then_some(Site {
            start: offset + start,
            kind: SiteKind::CommandArg { command, arg, word },
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
            // An unterminated plain (or raw) string on one line, that looks like a pattern.
            let completes = at_end
                && !info.closed
                && !info.continued
                && !info.quote.is_triple()
                && !letters.contains(['b', 'f'])
                && !prefix.contains('\\')
                && looks_like_pattern(prefix);
            completes.then_some(Site {
                start: offset + content.start,
                kind: SiteKind::TargetString { prefix },
            })
        }
        TokenKind::Ident | TokenKind::Keyword if at_end => {
            let word = last.text(text);
            match last_index.checked_sub(1) {
                Some(previous) if tokens.get(previous)?.kind == TokenKind::Dot => {
                    attr_site(text, &tokens, previous, offset + last.start, word)
                }
                Some(previous) => name_may_follow(text, &tokens, previous)
                    .then(|| name(offset + last.start, word)),
                None => Some(name(offset + last.start, word)),
            }
        }
        TokenKind::Dot => attr_site(text, &tokens, last_index, offset + text.len(), ""),
        _ => name_may_follow(text, &tokens, last_index).then(|| name(offset + text.len(), "")),
    }
}

fn name(start: usize, prefix: &str) -> Site<'_> {
    Site {
        start,
        kind: SiteKind::Name { prefix },
    }
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
        assert_eq!(site("f(▮"), name_at(2, ""));
        assert_eq!(site("f(a, ▮"), name_at(5, ""));
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
        assert_eq!(site("def f(a = g(b▮"), name_at(12, "b"));
        assert_eq!(site("lambda a: b▮"), name_at(10, "b"));
        assert_eq!(site("for x in y▮"), name_at(9, "y"));
        assert_eq!(site("for x in a, b▮"), name_at(12, "b"));
        assert_eq!(site("f(a, b▮"), name_at(5, "b"));
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
        assert_eq!(site(":l hel.bzl sym▮"), None);
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
            arg_at(
                9,
                CommandId::Cquery,
                ArgKind::Query(crate::commands::QueryDialect::Cquery),
                "//fo"
            )
        );
        assert_eq!(
            site(":uq :li▮"),
            arg_at(
                4,
                CommandId::Uquery,
                ArgKind::Query(crate::commands::QueryDialect::Uquery),
                ":li"
            )
        );
        assert_eq!(site(":cq dep▮"), None);
        assert_eq!(site(":cq deps(\"//a\", 1▮"), None);
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
        ];
        for s in samples {
            for pos in 0..=s.len() + 1 {
                let _ignored = classify(s, pos);
            }
        }
    }
}
