/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! `:help`: the commands, one command, or a topic.

use std::fmt::Write;

use buck2_repl_syntax::commands::COMMANDS;
use buck2_repl_syntax::commands::CommandId;
use buck2_repl_syntax::commands::CommandSpec;
use buck2_repl_syntax::commands::HELP_TOPICS;
use buck2_repl_syntax::commands::Priority;
use buck2_repl_syntax::commands::resolve_command;

/// The text of `:help [topic]`, or an error message (without `error: `) for an unknown topic.
pub(crate) fn help(topic: &str) -> Result<String, String> {
    let topic = topic.trim();
    match topic {
        "" => Ok(commands()),
        "keys" => Ok(KEYS.to_owned()),
        "patterns" => Ok(PATTERNS.to_owned()),
        _ => {
            let name = topic.strip_prefix(':').unwrap_or(topic);
            match resolve_command(name) {
                Ok(spec) if listed(spec) => Ok(command(spec)),
                _ => Err(format!(
                    "no help for `{topic}`: `:help` takes a command ({}) or a topic ({})",
                    COMMANDS
                        .iter()
                        .filter(|c| listed(c))
                        .map(|c| c.display_name())
                        .collect::<Vec<_>>()
                        .join(" "),
                    HELP_TOPICS.join(", ")
                )),
            }
        }
    }
}

/// The commands `:help` shows: those available (P1 commands are not implemented yet).
fn listed(spec: &CommandSpec) -> bool {
    !spec.hidden && spec.priority == Priority::P0
}

fn aliases(spec: &CommandSpec) -> String {
    spec.aliases
        .iter()
        .map(|a| format!(":{a}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The table of commands.
fn commands() -> String {
    let listed: Vec<&CommandSpec> = COMMANDS.iter().filter(|c| listed(c)).collect();
    let usage_width = listed.iter().map(|c| c.usage.len()).max().unwrap_or(0);
    let alias_width = listed.iter().map(|c| aliases(c).len()).max().unwrap_or(0);
    let mut out = String::new();
    out.push_str(
        "Anything that is not a command is Starlark, evaluated with `ctx` (a bxl.Context) bound;\n\
         the value of an input is printed and becomes `_`.\n\n\
         Commands (a unique prefix of a name works too, e.g. `:prov` for `:providers`):\n",
    );
    for c in &listed {
        // Writing to a `String` cannot fail.
        let _ignored = writeln!(
            out,
            "  {:<usage_width$}  {:<alias_width$}  {}",
            c.usage,
            aliases(c),
            c.summary
        );
    }
    out.push_str(
        "\n`:help <command>` for details, `:help keys` for the key bindings, \
         `:help patterns` for target patterns.",
    );
    out
}

/// Help on one command.
fn command(spec: &CommandSpec) -> String {
    let mut out = String::new();
    // Writing to a `String` cannot fail.
    let _ignored = write!(out, "{}", spec.usage);
    if !spec.aliases.is_empty() {
        let _ignored = write!(out, "    (also {})", aliases(spec));
    }
    let _ignored = write!(out, "\n{}.\n\n{}", spec.summary, wrap(details(spec.id)));
    out
}

/// Width of the paragraphs of help on a command.
const WRAP_COLUMNS: usize = 88;

/// Wraps the lines of `text` at [`WRAP_COLUMNS`], at spaces.
fn wrap(text: &str) -> String {
    let mut out = String::new();
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let mut width = 0;
        for (j, word) in line.split(' ').enumerate() {
            if j > 0 {
                if width + 1 + word.len() > WRAP_COLUMNS {
                    out.push('\n');
                    width = 0;
                } else {
                    out.push(' ');
                    width += 1;
                }
            }
            out.push_str(word);
            width += word.len();
        }
    }
    out
}

fn details(id: CommandId) -> &'static str {
    match id {
        CommandId::Help => {
            "Without an argument, lists the commands. With a command (its name, an alias or a \
             unique prefix, with or without the colon), shows help on it; `:help keys` shows \
             the key bindings and `:help patterns` explains target patterns."
        }
        CommandId::Quit => "Ends the session, like Ctrl-D on an empty line.",
        CommandId::Time => {
            "Runs the input (Starlark or a command), then prints how long it took: in total, \
             waiting for the daemon (for example while another buck2 command holds it), and \
             evaluating.\n\nExample: :time :cq deps(//...)"
        }
        CommandId::Type => {
            "Evaluates the expression and prints its type as the type checker sees it (for \
             example `list[str]`, or the signature of a function), followed by what `type()` \
             returns when that differs. The value does not become `_`."
        }
        CommandId::Print => {
            "Evaluates the expression and prints all of its value (up to 16 MiB), where the \
             echo of an input stops after 40 lines. The value becomes `_`: `:p _` shows all of \
             the last value."
        }
        CommandId::Json => {
            "Evaluates the expression and prints it as pretty JSON, or an error if it (or a \
             value in it) has no JSON form. The value becomes `_`."
        }
        CommandId::Doc => {
            "Shows the documentation of a function, type or namespace (`:doc len`, \
             `:doc ctx.cquery`), or, for any other value, of its type: `:doc ctx` shows every \
             method and attribute of bxl.Context."
        }
        CommandId::Load => {
            "With symbols, loads them from the module: `:load //pkg:defs.bzl f g` is \
             `load(\"//pkg:defs.bzl\", \"f\", \"g\")`. Without, imports every public symbol of \
             the module.\n\nThe module is a label (`//pkg:defs.bzl`, `cell//pkg:x.bxl`, \
             `:defs.bzl`) or a path relative to the session's directory (`defs.bzl`, \
             `sub/x.bxl`). `:reload` loads the modules again."
        }
        CommandId::Reload => {
            "Loads every module loaded so far (by `load()` or `:load`) again, in a new \
             transaction, so that edits to them are picked up, and binds their symbols again. \
             Values computed with the old versions are not recomputed."
        }
        CommandId::Reset => {
            "Drops every binding and loaded module and starts over as a new session would: the \
             prelude is imported and `ctx` is bound. This frees the session's heap."
        }
        CommandId::Uquery => {
            "Runs `ctx.uquery().eval(\"<query>\")`, an unconfigured query like `buck2 uquery`. \
             Patterns and files in it are relative to the session's directory (see \
             `:help patterns`). The result becomes `_`.\n\nExample: :uq deps(:lib, 1)"
        }
        CommandId::Cquery => {
            "Runs `ctx.cquery().eval(\"<query>\")`, a configured query like `buck2 cquery`, with \
             the session's target platform. Patterns and files in it are relative to the \
             session's directory (see `:help patterns`). The result becomes `_`.\n\n\
             Example: :cq kind(rule, deps(:lib))"
        }
        CommandId::Aquery => {
            "Runs `ctx.aquery().eval(\"<query>\")`, an action query like `buck2 aquery`. \
             Patterns in it are relative to the session's directory (see `:help patterns`). \
             The result becomes `_`."
        }
        CommandId::Providers => {
            "Analyzes the target, configured with the session's target platform, and shows \
             its providers: `ctx.analysis(ctx.configured_targets(\"<target>\")).providers()`. \
             For a pattern (`//pkg:`, `//pkg/...`), a dict of the providers of each target. \
             The result becomes `_`."
        }
        CommandId::Build => {
            "Builds the targets, materializes their outputs and prints one `label  path` line \
             per output. `_` becomes a dict of the output paths by label."
        }
        CommandId::Run => {
            "Builds the target and runs its `RunInfo` command in the current directory, with \
             the arguments after `--`. With `--print`, prints the command instead of running \
             it."
        }
        _ => "",
    }
}

const KEYS: &str = "\
Key bindings:
  Enter                 Submit the input if it is complete, otherwise start a new line
  Alt-Enter, Esc Enter  Start a new line
  Tab                   Indent (at the start of a line), otherwise complete
  Ctrl-C                Clear the input; while an input runs, interrupt it (a third
                        press ends the session)
  Ctrl-D                End the session (on an empty line)
  Up, Down              Previous and next input in the history
  Ctrl-R                Search the history
  Right, End            Accept the suggestion from the history (shown dimmed)
  Ctrl-L                Clear the screen

An input is complete unless a bracket or a triple-quoted string is open, a line ends with
`\\` or `:`, or it is a block (`def`, `for`, `if`, ...) whose last line is not empty: as in
Python, an empty line ends a block. A command (`:...`) is always one line.";

const PATTERNS: &str = "\
Target patterns are relative to the session's directory (shown in the prompt), as on the
buck2 command line:
  :lib             the target `lib` in the package of the session's directory
  sub:lib          the target `lib` in the package `sub` below it
  //pkg:lib        the target `lib` in the package `pkg` of the current cell
  cell//pkg:lib    a target in another cell
  //pkg:           every target in the package `pkg`
  //pkg/...        every target in `pkg` and the packages below it
  ...              every target at or below the session's directory

This holds for the commands that take targets or queries (:providers, :cquery, :uquery,
:aquery) and for everything `ctx` resolves: `ctx.configured_targets(\":lib\")`, patterns
and file names in `ctx.cquery().eval(...)`, and so on (`buck2 bxl` resolves these
against the cell root). A relative `load()` is relative to the session's directory, as
if the session were a .bxl file there.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_help() {
        let all = help("").unwrap();
        assert!(all.contains(":cquery <query>"), "{all}");
        assert!(!all.contains(":__complete"), "{all}");
        assert!(help(":cq").unwrap().starts_with(":cquery <query>"));
        assert!(help("reset").unwrap().starts_with(":reset"));
        assert!(help("keys").unwrap().contains("Ctrl-D"));
        assert!(help("patterns").unwrap().contains("//pkg/..."));
        assert!(help("zz").is_err());
        assert_eq!(wrap("a b"), "a b");
        let long = "word ".repeat(40);
        assert!(wrap(&long).lines().all(|l| l.len() <= WRAP_COLUMNS));
        // Every listed command has details.
        for c in COMMANDS.iter().filter(|c| listed(c)) {
            assert!(!details(c.id).is_empty(), "{}", c.name);
        }
    }
}
