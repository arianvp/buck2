/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Text helpers for completion candidates: which part of a pattern the daemon lists, how a
//! candidate is shown, and how a chosen candidate is inserted.

use crate::lexer::is_ident_continue;

/// Whether a module or a `.bxl` file is named by a label (`//pkg:x.bzl`, `cell//x:y.bzl`,
/// `:x.bzl`, `@cell//...`), not by a path relative to the working directory.
pub fn is_label(word: &str) -> bool {
    word.contains("//") || word.contains(':') || word.starts_with('@')
}

/// Whether the directories of `word`, a path relative to the working directory being typed
/// (the part before its last `/`), may be part of a module to load: buck2 takes only forward
/// relative paths (no `..`, `.` or empty parts). `:load` drops leading `./` before it loads
/// (`drop_dot_slash`).
pub fn is_loadable_dir(word: &str, drop_dot_slash: bool) -> bool {
    let mut word = word;
    while drop_dot_slash && let Some(rest) = word.strip_prefix("./") {
        word = rest;
    }
    match word.rfind('/') {
        Some(i) => word
            .get(..i)
            .unwrap_or("")
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".."),
        None => true,
    }
}

/// Whether the directories of `word`, a path relative to the working directory being typed,
/// go up (`../`, `../../lib/`): `:load` and `load()` at the prompt take such paths (they turn
/// them into labels), which buck2 does not. As for [`is_loadable_dir`], `drop_dot_slash` drops
/// leading `./`; the other parts are directory names.
pub fn is_upward_dir(word: &str, drop_dot_slash: bool) -> bool {
    let mut word = word;
    while drop_dot_slash && let Some(rest) = word.strip_prefix("./") {
        word = rest;
    }
    let Some(dir) = word.rfind('/').and_then(|i| word.get(..i)) else {
        return false;
    };
    let parts = || dir.split('/');
    parts().any(|part| part == "..") && parts().all(|part| !part.is_empty() && part != ".")
}

/// The end of the cell part of a pattern (`cell//`, `//`), or 0.
fn after_cell(word: &str) -> usize {
    word.find("//").map_or(0, |i| i + 2)
}

/// The part of a target pattern that names a listing of the daemon: the targets of a package
/// (`//pkg:` for `//pkg:na`) or the subtargets of a target (`//pkg:a[` for `//pkg:a[x`).
/// `None` if the pattern names directories (they are listed for the word typed).
pub fn target_listing(word: &str) -> Option<&str> {
    if let Some(i) = word.rfind('[') {
        return word.get(..i + 1);
    }
    let after_cell = after_cell(word);
    let colon = word.get(after_cell..)?.find(':')? + after_cell;
    word.get(..colon + 1)
}

/// The part of a module label that names a listing of the daemon: the files of a directory
/// (`//pkg:` for `//pkg:x.b`). `None` if the label names directories. After the colon of a
/// label comes a file name, never a path (buck2 rejects `//pkg:sub/x.bzl`): `//pkg:sub/x` is
/// listed as `//pkg:`, and nothing in it matches.
pub fn load_listing(word: &str) -> Option<&str> {
    let after_cell = after_cell(word);
    let colon = word.get(after_cell..)?.find(':')? + after_cell;
    word.get(..colon + 1)
}

/// What the list of candidates shows for a candidate: for a path (a target, a directory, a
/// package, a file), its last part (`a` for `//lib:a`, `sub/` for `//lib/sub/`, `[x]` for
/// `//lib:a[x]`, without a closing quote); for anything else, the candidate itself (without a
/// closing quote).
pub fn short_display(replacement: &str, is_path: bool) -> String {
    if !is_path {
        // A candidate in a string (a symbol of `load`) ends with the closing quote, which the
        // listing does not show.
        let body = replacement.trim_end_matches(['"', '\'']);
        return if body.is_empty() { replacement } else { body }.to_owned();
    }
    let body = replacement.trim_end_matches(['"', '\'']);
    if body.ends_with(']')
        && let Some(i) = body.rfind('[')
    {
        return body.get(i..).unwrap_or(body).to_owned();
    }
    let (core, suffix) = match body.strip_suffix(['/', ':']) {
        Some(core) => (core, body.get(core.len()..).unwrap_or("")),
        None => (body, ""),
    };
    let name = core
        .rfind(['/', ':'])
        .map_or(core, |i| core.get(i + 1..).unwrap_or(""));
    if name.is_empty() {
        replacement.to_owned()
    } else {
        format!("{name}{suffix}")
    }
}

/// How to insert a whole candidate, `elected`, at the cursor `end` of `text`, when it is chosen:
/// how many bytes after the cursor it replaces too (the rest of the identifier under the cursor,
/// if the candidate ends with an identifier: `ctx.cq▮uery()` + `cquery(` gives `ctx.cquery()`),
/// what to insert, and how many bytes to move the cursor past after it: the `(`, `=` or quote
/// that the candidate ends with, when it follows already (after spaces for `=`), is not
/// repeated.
pub fn insertion<'a>(text: &str, end: usize, elected: &'a str) -> (usize, &'a str, usize) {
    let after = text.get(end..).unwrap_or("");
    let core = elected.trim_end_matches(['(', '=', '"', '\'']);
    let replaced = if core.ends_with(is_ident_continue) {
        after
            .char_indices()
            .find(|(_, c)| !is_ident_continue(*c))
            .map_or(after.len(), |(i, _)| i)
    } else {
        0
    };
    let rest = after.get(replaced..).unwrap_or("");
    let Some(last) = elected.chars().last() else {
        return (replaced, elected, 0);
    };
    let without_last = elected
        .get(..elected.len() - last.len_utf8())
        .unwrap_or(elected);
    match last {
        '(' | '"' | '\'' if rest.starts_with(last) => (replaced, without_last, last.len_utf8()),
        '=' => {
            let spaces = rest.len() - rest.trim_start_matches(' ').len();
            if rest.get(spaces..).is_some_and(|r| r.starts_with('=')) {
                (replaced, without_last, spaces + 1)
            } else {
                (replaced, elected, 0)
            }
        }
        _ => (replaced, elected, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_label() {
        for yes in ["//a", "cell//a", ":a.bzl", "@c//a", "a:b"] {
            assert!(is_label(yes), "{yes}");
        }
        for no in ["", "a", "a/b.bzl", "./a.bzl"] {
            assert!(!is_label(no), "{no}");
        }
    }

    #[test]
    fn test_is_loadable_dir() {
        for yes in ["", "a", "pkg/", "pkg/he", "a/b/c.bzl", ".hidden", ".."] {
            assert!(is_loadable_dir(yes, false), "{yes}");
        }
        for no in ["../", "../x", "a/../b", "./", "./x", "a/./b", "/a", "a//b"] {
            assert!(!is_loadable_dir(no, false), "{no}");
        }
        assert!(is_loadable_dir("./", true));
        assert!(is_loadable_dir("././pkg/he", true));
        assert!(!is_loadable_dir("./../x", true));
        assert!(!is_loadable_dir("pkg/./x", true));
    }

    #[test]
    fn test_is_upward_dir() {
        for yes in ["../", "../x", "../../lib/he", "a/../b", "../a/"] {
            assert!(is_upward_dir(yes, false), "{yes}");
        }
        for no in [
            "",
            "..",
            "a",
            "pkg/",
            "./../x",
            ".././/x",
            "/../x",
            "..//x",
            "pkg/./../x",
        ] {
            assert!(!is_upward_dir(no, false), "{no}");
        }
        assert!(is_upward_dir("./../x", true));
        assert!(!is_upward_dir("./pkg/x", true));
    }

    #[test]
    fn test_listings() {
        assert_eq!(target_listing("//pkg:na"), Some("//pkg:"));
        assert_eq!(target_listing(":na"), Some(":"));
        assert_eq!(target_listing("cell//a/b:"), Some("cell//a/b:"));
        assert_eq!(target_listing("//x:y[a"), Some("//x:y["));
        assert_eq!(target_listing("//x:y[a][b"), Some("//x:y[a]["));
        assert_eq!(target_listing("//pkg/su"), None);
        assert_eq!(target_listing("cell//"), None);
        assert_eq!(target_listing(""), None);
        assert_eq!(load_listing("//pkg:he"), Some("//pkg:"));
        assert_eq!(load_listing("//pkg:sub/x"), Some("//pkg:"));
        assert_eq!(load_listing("@cell//a:b"), Some("@cell//a:"));
        assert_eq!(load_listing(":x"), Some(":"));
        assert_eq!(load_listing("//pk"), None);
        assert_eq!(load_listing("//a/b"), None);
    }

    #[test]
    fn test_short_display() {
        assert_eq!(short_display("//lib:a", true), "a");
        assert_eq!(short_display("//lib:a\"", true), "a");
        assert_eq!(short_display(":a", true), "a");
        assert_eq!(short_display("//x:y[sub]", true), "[sub]");
        assert_eq!(short_display("//lib/sub/", true), "sub/");
        assert_eq!(short_display("//lib:", true), "lib:");
        assert_eq!(short_display("//lib/...", true), "...");
        assert_eq!(short_display("//pkg:helpers.bxl\"", true), "helpers.bxl");
        assert_eq!(short_display("pkg/helpers.bxl", true), "helpers.bxl");
        assert_eq!(short_display("//:", true), "//:");
        assert_eq!(short_display("//", true), "//");
        assert_eq!(short_display("", true), "");
        assert_eq!(short_display("ctx", false), "ctx");
        assert_eq!(short_display("a:b", false), "a:b");
        assert_eq!(short_display("platform\"", false), "platform");
        assert_eq!(short_display("\"", false), "\"");
    }

    #[test]
    fn test_insertion() {
        // The rest of the identifier is replaced, the `(` that follows is not repeated.
        assert_eq!(insertion("ctx.cquery()", 6, "cquery("), (4, "cquery", 1));
        assert_eq!(insertion("ctx.cq", 6, "cquery("), (0, "cquery(", 0));
        assert_eq!(insertion("ctx.cq x", 6, "cquery("), (0, "cquery(", 0));
        // Keyword arguments.
        assert_eq!(insertion("f(tar = 1)", 5, "target="), (0, "target", 2));
        assert_eq!(insertion("f(tar= 1)", 5, "target="), (0, "target", 1));
        assert_eq!(insertion("f(tarx)", 5, "target="), (1, "target=", 0));
        // Closing quotes.
        assert_eq!(insertion("a(\"//l\")", 6, "//lib:a\""), (0, "//lib:a", 1));
        assert_eq!(insertion("a(\"//l", 6, "//lib:a\""), (0, "//lib:a\"", 0));
        assert_eq!(
            insertion("a(\"//lx\")", 6, "//lib:lx\""),
            (1, "//lib:lx", 1)
        );
        // Not an identifier: nothing after the cursor is replaced.
        assert_eq!(insertion("//li b", 4, "//lib/"), (0, "//lib/", 0));
        // Odd inputs.
        assert_eq!(insertion("é", 0, "é"), (0, "é", 0));
        assert_eq!(insertion("abc", 10, "x"), (0, "x", 0));
        assert_eq!(insertion("abc", 1, ""), (0, "", 0));
        assert_eq!(insertion("aé(", 1, "x("), (0, "x(", 0));
    }
}
