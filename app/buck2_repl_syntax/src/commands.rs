/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! The meta-command table (`:help`, `:build`, ...) and its parser.
//!
//! A meta-command is an input whose first non-space character is `:`. The command token is
//! `!` or `?`, or a run of `[A-Za-z_]`. An exact name or alias wins; otherwise a unique prefix
//! of a (non-hidden) name is accepted. The rest of the input is the argument.

use std::borrow::Cow;
use std::fmt;

/// The `root` of a `NAME` completion request for the argument of `:who`: the names `:who` lists
/// (the session's own bindings, without the globals, the prelude's symbols, `ctx` and `_`).
pub const WHO_NAMES_ROOT: &str = ":who";

/// Every meta-command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandId {
    Help,
    Quit,
    Time,
    Type,
    Print,
    Json,
    Doc,
    Load,
    Reload,
    Reset,
    Uquery,
    Cquery,
    Aquery,
    Providers,
    Build,
    Run,
    Complete,
    Bxl,
    Info,
    Set,
    Who,
    Hist,
    Shell,
    Edit,
    Ls,
    Qdoc,
    Locate,
    Edited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryDialect {
    Uquery,
    Cquery,
    Aquery,
}

impl QueryDialect {
    /// The `bxl.Context` method that returns the query context (`ctx.cquery()`).
    pub fn ctx_method(self) -> &'static str {
        match self {
            QueryDialect::Uquery => "uquery",
            QueryDialect::Cquery => "cquery",
            QueryDialect::Aquery => "aquery",
        }
    }
}

/// What a command's argument is. Drives argument completion and parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArgKind {
    /// No argument.
    None,
    /// A command name or help topic.
    Topic,
    /// Any REPL input, including another meta-command (`:time`).
    Input,
    /// A Starlark expression.
    Expr,
    /// A query, passed raw.
    Query(QueryDialect),
    /// Exactly one target pattern.
    Target,
    /// One or more target patterns, shell-split.
    Targets,
    /// `[--print] <target> [-- args…]`, shell-split (see [`parse_run_args`]).
    Run,
    /// A module to load: a label or a path (`:load <label> [symbol…]`).
    Path,
    /// A file to edit: a path, a target (its build file) or a module label (`:edit`).
    File,
    /// A JSON object.
    Json,
    /// `<file.bxl:function> [-- args…]`.
    BxlLabel,
    /// `[key [value]]`.
    Setting,
    /// Globs over binding names.
    Glob,
    /// A count.
    Count,
    /// A shell command, passed raw.
    Shell,
    /// A package.
    Package,
    /// A query function name.
    QueryFunction,
}

/// Where a command is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Handler {
    /// By the client alone; the daemon rejects it.
    Client,
    /// By the daemon.
    Server,
    /// By both: the daemon does the work and the client finishes it (`:run` runs the command
    /// the daemon built; `:set` has keys on both sides).
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Priority {
    P0,
    P1,
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct CommandSpec {
    pub id: CommandId,
    /// The name, without the colon.
    pub name: &'static str,
    /// Other names, without the colon. Aliases only match exactly.
    pub aliases: &'static [&'static str],
    /// One-line syntax, e.g. `:type <expr>`.
    pub usage: &'static str,
    /// One-line description.
    pub summary: &'static str,
    pub arg: ArgKind,
    /// The argument must not be empty.
    pub arg_required: bool,
    pub handler: Handler,
    pub priority: Priority,
    /// Not listed by `:help` and not matched by prefix.
    pub hidden: bool,
}

impl CommandSpec {
    /// The name with its colon, e.g. `:build`.
    pub fn display_name(&self) -> String {
        format!(":{}", self.name)
    }
}

macro_rules! command {
    ($id:ident, $name:literal, [$($alias:literal),*], $usage:literal, $summary:literal, $arg:expr, $required:literal, $handler:ident, $priority:ident $(, $hidden:ident)?) => {
        CommandSpec {
            id: CommandId::$id,
            name: $name,
            aliases: &[$($alias),*],
            usage: $usage,
            summary: $summary,
            arg: $arg,
            arg_required: $required,
            handler: Handler::$handler,
            priority: Priority::$priority,
            hidden: command!(@hidden $($hidden)?),
        }
    };
    (@hidden) => { false };
    (@hidden hidden) => { true };
}

/// The meta-command table, in `:help` order.
pub static COMMANDS: &[CommandSpec] = &[
    command!(
        Help,
        "help",
        ["h", "?"],
        ":help [command|keys|patterns]",
        "Show the commands, or help on one command or topic",
        ArgKind::Topic,
        false,
        Client,
        P0
    ),
    command!(
        Quit,
        "quit",
        ["q"],
        ":quit",
        "End the session (same as Ctrl-D)",
        ArgKind::None,
        false,
        Client,
        P0
    ),
    command!(
        Time,
        "time",
        [],
        ":time <input>",
        "Run an input, then print how long it took",
        ArgKind::Input,
        true,
        Client,
        P0
    ),
    command!(
        Type,
        "type",
        ["t"],
        ":type <expr>",
        "Show the type of an expression (does not bind `_`)",
        ArgKind::Expr,
        true,
        Server,
        P0
    ),
    command!(
        Print,
        "print",
        ["p"],
        ":print <expr>",
        "Print the whole value of an expression; binds `_`",
        ArgKind::Expr,
        true,
        Server,
        P0
    ),
    command!(
        Json,
        "json",
        ["j"],
        ":json <expr>",
        "Print an expression as pretty JSON; binds `_`",
        ArgKind::Expr,
        true,
        Server,
        P0
    ),
    command!(
        Doc,
        "doc",
        ["d"],
        ":doc <expr>",
        "Show the documentation of a value, or of its type",
        ArgKind::Expr,
        true,
        Server,
        P0
    ),
    command!(
        Load,
        "load",
        ["l"],
        ":load <label> [symbol...]",
        "Load symbols from a .bzl or .bxl file (all public ones if none are named)",
        ArgKind::Path,
        true,
        Server,
        P0
    ),
    command!(
        Reload,
        "reload",
        ["r"],
        ":reload",
        "Load the files loaded so far again, picking up changes",
        ArgKind::None,
        false,
        Server,
        P0
    ),
    command!(
        Reset,
        "reset",
        [],
        ":reset",
        "Start over: drop all bindings and loads",
        ArgKind::None,
        false,
        Server,
        P0
    ),
    command!(
        Uquery,
        "uquery",
        ["uq"],
        ":uquery <query>",
        "Run an unconfigured query; binds `_`",
        ArgKind::Query(QueryDialect::Uquery),
        true,
        Server,
        P0
    ),
    command!(
        Cquery,
        "cquery",
        ["cq"],
        ":cquery <query>",
        "Run a configured query; binds `_`",
        ArgKind::Query(QueryDialect::Cquery),
        true,
        Server,
        P0
    ),
    command!(
        Aquery,
        "aquery",
        ["aq"],
        ":aquery <query>",
        "Run an action query; binds `_`",
        ArgKind::Query(QueryDialect::Aquery),
        true,
        Server,
        P0
    ),
    command!(
        Providers,
        "providers",
        ["pv"],
        ":providers <target>",
        "Analyze a target and show its providers; binds `_`",
        ArgKind::Target,
        true,
        Server,
        P0
    ),
    command!(
        Build,
        "build",
        ["b"],
        ":build <pattern>...",
        "Build targets and print their outputs; binds `_`",
        ArgKind::Targets,
        true,
        Server,
        P0
    ),
    command!(
        Run,
        "run",
        [],
        ":run [--print] <target> [-- args...]",
        "Build a target and run it (with --print, only print the command)",
        ArgKind::Run,
        true,
        Both,
        P0
    ),
    command!(
        Complete,
        "__complete",
        [],
        ":__complete {\"buf\": ..., \"pos\": N}",
        "Print the completions of a buffer as JSON",
        ArgKind::Json,
        true,
        Client,
        P0,
        hidden
    ),
    command!(
        Bxl,
        "bxl",
        [],
        ":bxl <file.bxl:function> [-- args...]",
        "Run a BXL function, with actions and outputs",
        ArgKind::BxlLabel,
        true,
        Server,
        P1
    ),
    command!(
        Info,
        "info",
        ["i"],
        ":info <target>",
        "Show a target's rule type, build file, attributes and deps",
        ArgKind::Target,
        true,
        Server,
        P1
    ),
    command!(
        Set,
        "set",
        [],
        ":set [key [value...]]",
        "Show or change the session's settings (target platform, colour, ...)",
        ArgKind::Setting,
        false,
        Both,
        P1
    ),
    command!(
        Who,
        "who",
        ["vars"],
        ":who [glob...]",
        "List the session's bindings with their types",
        ArgKind::Glob,
        false,
        Server,
        P1
    ),
    command!(
        Hist,
        "hist",
        ["history"],
        ":hist [n]",
        "Show the inputs of the session, numbered like `<repl:N>`",
        ArgKind::Count,
        false,
        Client,
        P1
    ),
    command!(
        Shell,
        "!",
        ["shell"],
        ":!<command>",
        "Run a shell command in the current directory",
        ArgKind::Shell,
        true,
        Client,
        P1
    ),
    command!(
        Edit,
        "edit",
        ["e"],
        ":edit [path|target]",
        "Edit a file or a target's build file, or a scratch buffer to run",
        ArgKind::File,
        false,
        Client,
        P1
    ),
    command!(
        Ls,
        "ls",
        [],
        ":ls [package]",
        "List the targets of a package, with their rule types",
        ArgKind::Package,
        false,
        Server,
        P1
    ),
    command!(
        Qdoc,
        "qdoc",
        [],
        ":qdoc [function|language]",
        "Show the documentation of the query functions",
        ArgKind::QueryFunction,
        false,
        Server,
        P1
    ),
    command!(
        Locate,
        "__locate",
        [],
        ":__locate <target|module>",
        "Print where a target is defined, or where a module is, as JSON",
        ArgKind::Target,
        true,
        Server,
        P1,
        hidden
    ),
    command!(
        Edited,
        "__edited",
        [],
        ":__edited <absolute path>",
        "Load the loaded modules again if the file is one of them (or loaded by one)",
        ArgKind::File,
        true,
        Server,
        P1,
        hidden
    ),
];

/// Topics of `:help` besides command names.
pub const HELP_TOPICS: &[&str] = &["keys", "patterns"];

/// The table entry of `id`.
pub fn command(id: CommandId) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|c| c.id == id)
}

/// The command whose name or alias is exactly `name` (without the colon).
pub fn lookup_exact(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name))
}

/// The pieces of a meta-command input, before the command is resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandToken<'a> {
    /// The command token as typed, without the colon (may be empty).
    pub token: &'a str,
    /// Byte offset of the token (just after the colon).
    pub token_start: usize,
    /// Byte offset just after the token: the argument is `input[arg_start..]`.
    pub arg_start: usize,
}

/// Splits a meta-command input into its token and argument. `None` if `input` is not a
/// meta-command (its first non-space character is not `:`).
pub fn split_command_token(input: &str) -> Option<CommandToken<'_>> {
    let lead = input.len() - input.trim_start().len();
    let after = input.get(lead..)?.strip_prefix(':')?;
    let token_start = lead + 1;
    let token_len = match after.as_bytes().first() {
        Some(b'!' | b'?') => 1,
        _ => after
            .bytes()
            .take_while(|b| b.is_ascii_alphabetic() || *b == b'_')
            .count(),
    };
    let arg_start = token_start + token_len;
    Some(CommandToken {
        token: input.get(token_start..arg_start)?,
        token_start,
        arg_start,
    })
}

/// A parsed meta-command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand<'a> {
    pub spec: &'static CommandSpec,
    /// The command token as typed, without the colon.
    pub token: &'a str,
    /// The argument: the rest of the input, trimmed, with `\`-newline continuations replaced by
    /// spaces.
    pub arg: Cow<'a, str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandError {
    /// `:` without a command name.
    MissingName,
    Unknown {
        token: String,
    },
    Ambiguous {
        token: String,
        /// Names (without colons) of the commands the token is a prefix of.
        candidates: Vec<&'static str>,
    },
    MissingArgument {
        spec: &'static CommandSpec,
    },
    UnexpectedArgument {
        spec: &'static CommandSpec,
    },
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandError::MissingName => write!(
                f,
                "missing command name after `:` (type :help for the list of commands)"
            ),
            CommandError::Unknown { token } => write!(
                f,
                "unknown command `:{token}` (type :help for the list of commands)"
            ),
            CommandError::Ambiguous { token, candidates } => {
                write!(f, "ambiguous command `:{token}`: could be ")?;
                for (i, c) in candidates.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, ":{c}")?;
                }
                Ok(())
            }
            CommandError::MissingArgument { spec } => write!(
                f,
                "`:{}` needs an argument; usage: {}",
                spec.name, spec.usage
            ),
            CommandError::UnexpectedArgument { spec } => {
                write!(f, "`:{}` takes no argument", spec.name)
            }
        }
    }
}

impl std::error::Error for CommandError {}

/// Resolves a command token (without the colon): an exact name or alias, otherwise a unique
/// prefix of a name.
pub fn resolve_command(token: &str) -> Result<&'static CommandSpec, CommandError> {
    if token.is_empty() {
        return Err(CommandError::MissingName);
    }
    if let Some(spec) = lookup_exact(token) {
        return Ok(spec);
    }
    let candidates: Vec<&'static CommandSpec> = COMMANDS
        .iter()
        .filter(|c| !c.hidden && c.name.starts_with(token))
        .collect();
    match candidates.as_slice() {
        [spec] => Ok(spec),
        [] => Err(CommandError::Unknown {
            token: token.to_owned(),
        }),
        many => Err(CommandError::Ambiguous {
            token: token.to_owned(),
            candidates: many.iter().map(|c| c.name).collect(),
        }),
    }
}

/// Parses a meta-command. `Ok(None)` if `input` is not one (its first non-space character is
/// not `:`).
pub fn parse_command(input: &str) -> Result<Option<ParsedCommand<'_>>, CommandError> {
    let Some(token) = split_command_token(input) else {
        return Ok(None);
    };
    let spec = resolve_command(token.token)?;
    let arg = join_continuations(input.get(token.arg_start..).unwrap_or(""));
    let arg = match arg {
        Cow::Borrowed(s) => Cow::Borrowed(s.trim()),
        Cow::Owned(s) => Cow::Owned(s.trim().to_owned()),
    };
    if spec.arg == ArgKind::None && !arg.is_empty() {
        return Err(CommandError::UnexpectedArgument { spec });
    }
    if spec.arg_required && arg.is_empty() {
        return Err(CommandError::MissingArgument { spec });
    }
    Ok(Some(ParsedCommand {
        spec,
        token: token.token,
        arg,
    }))
}

/// Replaces each `\`-newline line continuation with a space.
pub fn join_continuations(s: &str) -> Cow<'_, str> {
    if s.contains("\\\n") || s.contains("\\\r\n") {
        Cow::Owned(s.replace("\\\r\n", " ").replace("\\\n", " "))
    } else {
        Cow::Borrowed(s)
    }
}

/// Why command arguments could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgError {
    /// Unbalanced quotes or a trailing backslash.
    Quoting,
    MissingTarget,
    ExtraArgument(String),
    UnknownFlag(String),
    /// `:bxl` without `<file.bxl>:<function>`.
    MissingBxlFunction,
    /// A word after the BXL function, before `--`.
    ExtraBxlArgument(String),
    /// `:set` of a setting that does not exist.
    UnknownSetting(String),
    /// `:hist` of something that is not a count.
    NotACount(String),
}

impl fmt::Display for ArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArgError::Quoting => write!(
                f,
                "could not split the arguments: unbalanced quotes or a trailing backslash"
            ),
            ArgError::MissingTarget => write!(f, "missing target"),
            ArgError::ExtraArgument(a) => write!(
                f,
                "unexpected argument `{a}` (arguments for the program go after `--`)"
            ),
            ArgError::UnknownFlag(a) => write!(f, "unknown flag `{a}`"),
            ArgError::MissingBxlFunction => {
                write!(f, "missing BXL function (`<file.bxl>:<function>`)")
            }
            ArgError::ExtraBxlArgument(a) => write!(
                f,
                "unexpected argument `{a}` (arguments for the BXL function go after `--`)"
            ),
            ArgError::UnknownSetting(key) => {
                write!(f, "unknown setting `{key}`; the settings are ")?;
                for (i, s) in SETTINGS.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", s.name)?;
                }
                Ok(())
            }
            ArgError::NotACount(a) => write!(f, "`{a}` is not a number of inputs"),
        }
    }
}

impl std::error::Error for ArgError {}

/// Splits command arguments like a POSIX shell.
pub fn split_args(arg: &str) -> Result<Vec<String>, ArgError> {
    shlex::split(arg).ok_or(ArgError::Quoting)
}

/// Splits `words` at the first `--`: the words before it, and the words after it.
pub fn split_dashdash(mut words: Vec<String>) -> (Vec<String>, Vec<String>) {
    match words.iter().position(|w| w == "--") {
        Some(i) => {
            let after = words.split_off(i).into_iter().skip(1).collect();
            (words, after)
        }
        None => (words, Vec::new()),
    }
}

/// The argument of `:run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunArgs {
    /// `--print`: print the command instead of running it.
    pub print: bool,
    pub target: String,
    /// Arguments for the program (after `--`).
    pub args: Vec<String>,
}

/// Parses `[--print] <target> [-- args…]`.
pub fn parse_run_args(arg: &str) -> Result<RunArgs, ArgError> {
    let (before, args) = split_dashdash(split_args(arg)?);
    let mut print = false;
    let mut target = None;
    for word in before {
        if word == "--print" {
            print = true;
        } else if word.starts_with('-') {
            return Err(ArgError::UnknownFlag(word));
        } else if target.is_none() {
            target = Some(word);
        } else {
            return Err(ArgError::ExtraArgument(word));
        }
    }
    Ok(RunArgs {
        print,
        target: target.ok_or(ArgError::MissingTarget)?,
        args,
    })
}

/// The argument of `:load`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadArgs {
    /// The module, as typed: a label (`//pkg:x.bzl`, `:x.bzl`) or a path (`x.bzl`).
    pub module: String,
    /// The symbols to load; empty means all public symbols.
    pub symbols: Vec<String>,
}

/// Parses `<label> [symbol…]`.
pub fn parse_load_args(arg: &str) -> Result<LoadArgs, ArgError> {
    let mut words = split_args(arg)?.into_iter();
    let module = words.next().ok_or(ArgError::MissingTarget)?;
    Ok(LoadArgs {
        module,
        symbols: words.collect(),
    })
}

/// The argument of `:bxl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BxlArgs {
    /// `<file.bxl>:<function>`, as typed: the file is a label (`//pkg:x.bxl`, `//pkg/x.bxl`,
    /// `:x.bxl`) or a path (`x.bxl`, `sub/x.bxl`), as `buck2 bxl` takes it.
    pub label: String,
    /// The function's command-line arguments (after `--`).
    pub args: Vec<String>,
}

/// Parses `<file.bxl:function> [-- args…]`.
pub fn parse_bxl_args(arg: &str) -> Result<BxlArgs, ArgError> {
    let (before, args) = split_dashdash(split_args(arg)?);
    let mut label = None;
    for word in before {
        if label.is_some() {
            return Err(ArgError::ExtraBxlArgument(word));
        } else if word.starts_with('-') {
            return Err(ArgError::UnknownFlag(word));
        } else {
            label = Some(word);
        }
    }
    Ok(BxlArgs {
        label: label.ok_or(ArgError::MissingBxlFunction)?,
        args,
    })
}

/// Splits a word of `:bxl` that names a function of a `.bxl` file (`//pkg:x.bxl:ma`) into the
/// file and the (partial) function name. `None` if the word does not name a function yet
/// (`//pkg:x.b`, `sub/x.bxl`).
pub fn split_bxl_function(word: &str) -> Option<(&str, &str)> {
    let (file, function) = word.rsplit_once(':')?;
    file.ends_with(".bxl").then_some((file, function))
}

/// Parses the argument of `:hist`: nothing (every input) or a count.
pub fn parse_count(arg: &str) -> Result<Option<usize>, ArgError> {
    let arg = arg.trim();
    if arg.is_empty() {
        return Ok(None);
    }
    match arg.parse::<usize>() {
        Ok(n) => Ok(Some(n)),
        Err(_) => Err(ArgError::NotACount(arg.to_owned())),
    }
}

/// Where a setting of `:set` lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingSide {
    /// The daemon keeps it (it changes what the session computes).
    Server,
    /// The client keeps it (it changes how results are shown).
    Client,
}

/// A setting of `:set`.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct SettingSpec {
    pub name: &'static str,
    pub side: SettingSide,
    /// The values it takes, e.g. `auto|on|off`.
    pub values: &'static str,
    /// The words it takes, for completion (empty if it takes any word).
    pub choices: &'static [&'static str],
    pub summary: &'static str,
}

/// The settings of `:set`, in the order `:set` lists them.
pub static SETTINGS: &[SettingSpec] = &[
    SettingSpec {
        name: "target_platforms",
        side: SettingSide::Server,
        values: "<target>|\"\"",
        choices: &[],
        summary: "the target platform that configures targets (`ctx`, queries, :build, :run, \
                  :bxl), as --target-platforms; \"\" for the default",
    },
    SettingSpec {
        name: "modifiers",
        side: SettingSide::Server,
        values: "<modifier>...|\"\"",
        choices: &[],
        summary: "the configuration modifiers of every target, as -m; \"\" for none",
    },
    SettingSpec {
        name: "color",
        side: SettingSide::Client,
        values: "auto|on|off",
        choices: &["auto", "on", "off"],
        summary: "colour errors, notes and the input being typed (auto: interactively, on a \
                  terminal, unless NO_COLOR is set)",
    },
    SettingSpec {
        name: "timing",
        side: SettingSide::Client,
        values: "auto|on|off",
        choices: &["auto", "on", "off"],
        summary: "show how long inputs take (auto: interactively, those that take a second or \
                  more; on: every input, as :time does)",
    },
    SettingSpec {
        name: "completion_timeout_ms",
        side: SettingSide::Client,
        values: "<milliseconds>|auto",
        choices: &["auto"],
        summary: "how long Tab waits for the daemon (auto: 500 for names, 1000 for targets and \
                  modules)",
    },
];

/// Width of the name column when `:set` lists the settings.
pub const SETTING_NAME_WIDTH: usize = 22;

/// The setting named exactly `name`.
pub fn setting(name: &str) -> Option<&'static SettingSpec> {
    SETTINGS.iter().find(|s| s.name == name)
}

/// The argument of `:set`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetArgs {
    /// `None`: list every setting.
    pub key: Option<&'static SettingSpec>,
    /// `None`: show the setting. `Some`: its new value, shell-split (`[""]` for `""`).
    pub value: Option<Vec<String>>,
}

/// Parses `[key [value...]]`.
pub fn parse_set_args(arg: &str) -> Result<SetArgs, ArgError> {
    let mut words = split_args(arg)?.into_iter();
    let Some(key) = words.next() else {
        return Ok(SetArgs {
            key: None,
            value: None,
        });
    };
    let Some(spec) = setting(&key) else {
        return Err(ArgError::UnknownSetting(key));
    };
    let value: Vec<String> = words.collect();
    Ok(SetArgs {
        key: Some(spec),
        value: (!value.is_empty()).then_some(value),
    })
}

/// A line of the listing of `:set`: the name of the setting and its value.
pub fn setting_line(name: &str, value: &str) -> String {
    format!("{name:<SETTING_NAME_WIDTH$} {value}")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn parse(input: &str) -> Result<Option<ParsedCommand<'_>>, CommandError> {
        parse_command(input)
    }

    fn parsed_id(input: &str) -> CommandId {
        parse(input).unwrap().unwrap().spec.id
    }

    #[test]
    fn test_table_is_consistent() {
        let mut names = HashSet::new();
        let mut ids = HashSet::new();
        for c in COMMANDS {
            assert!(ids.insert(c.id), "duplicate id {:?}", c.id);
            assert!(names.insert(c.name), "duplicate name {}", c.name);
            for a in c.aliases {
                assert!(names.insert(a), "duplicate alias {a}");
            }
            assert!(c.usage.starts_with(&c.display_name()), "{}", c.usage);
            assert_eq!(command(c.id), Some(c));
            // Every name and alias parses back to its command.
            let input = if c.arg == ArgKind::None {
                format!(":{}", c.name)
            } else {
                format!(":{} x", c.name)
            };
            assert_eq!(parsed_id(&input), c.id);
            for a in c.aliases {
                assert_eq!(resolve_command(a), Ok(c));
            }
            if c.arg == ArgKind::None {
                assert!(!c.arg_required);
            }
        }
        assert_eq!(COMMANDS.len(), 28);
        assert_eq!(
            COMMANDS
                .iter()
                .filter(|c| c.priority == Priority::P0)
                .count(),
            17
        );
    }

    #[test]
    fn test_not_a_command() {
        assert_eq!(parse("x = 1"), Ok(None));
        assert_eq!(parse(""), Ok(None));
        assert_eq!(parse("  "), Ok(None));
        assert_eq!(parse("x[1:2]"), Ok(None));
    }

    #[test]
    fn test_exact_and_alias() {
        let c = parse(":build //foo:bar //baz/...").unwrap().unwrap();
        assert_eq!(c.spec.id, CommandId::Build);
        assert_eq!(c.token, "build");
        assert_eq!(c.arg, "//foo:bar //baz/...");
        let c = parse("  :b   //foo  ").unwrap().unwrap();
        assert_eq!(c.spec.id, CommandId::Build);
        assert_eq!(c.token, "b");
        assert_eq!(c.arg, "//foo");
        assert_eq!(parsed_id(":cq deps(//:a)"), CommandId::Cquery);
        assert_eq!(parsed_id(":h"), CommandId::Help);
        assert_eq!(parsed_id(":?"), CommandId::Help);
        assert_eq!(parsed_id(":? build"), CommandId::Help);
        assert_eq!(parsed_id(":q"), CommandId::Quit);
        assert_eq!(parsed_id(":vars"), CommandId::Who);
        assert_eq!(parsed_id(":__complete {}"), CommandId::Complete);
        // The argument may follow the token without a space.
        let c = parse(":t(1, 2)").unwrap().unwrap();
        assert_eq!(c.spec.id, CommandId::Type);
        assert_eq!(c.arg, "(1, 2)");
        // Exact beats prefix: `:r` is `:reload`'s alias although `:run` and `:reset` exist.
        assert_eq!(parsed_id(":r"), CommandId::Reload);
        assert_eq!(parsed_id(":run //:x"), CommandId::Run);
    }

    #[test]
    fn test_unique_prefix() {
        assert_eq!(parsed_id(":bu //foo"), CommandId::Build);
        assert_eq!(parsed_id(":bx f.bxl:main"), CommandId::Bxl);
        assert_eq!(parsed_id(":prov //:x"), CommandId::Providers);
        assert_eq!(parsed_id(":ty 1"), CommandId::Type);
        assert_eq!(parsed_id(":ti 1"), CommandId::Time);
        assert_eq!(parsed_id(":c deps(x)"), CommandId::Cquery);
        // Aliases only match exactly.
        assert_eq!(
            parse(":histo"),
            Err(CommandError::Unknown {
                token: "histo".to_owned()
            })
        );
        // Hidden commands only match exactly.
        assert_eq!(
            parse(":__"),
            Err(CommandError::Unknown {
                token: "__".to_owned()
            })
        );
    }

    #[test]
    fn test_ambiguous() {
        let err = parse(":re").unwrap_err();
        assert_eq!(
            err,
            CommandError::Ambiguous {
                token: "re".to_owned(),
                candidates: vec!["reload", "reset"],
            }
        );
        assert_eq!(
            err.to_string(),
            "ambiguous command `:re`: could be :reload, :reset"
        );
        assert_eq!(
            parse(":pr x").unwrap_err(),
            CommandError::Ambiguous {
                token: "pr".to_owned(),
                candidates: vec!["print", "providers"],
            }
        );
        assert!(matches!(
            parse(":t").unwrap_err(),
            CommandError::MissingArgument { .. }
        ));
    }

    #[test]
    fn test_unknown() {
        let err = parse(":zz").unwrap_err();
        assert_eq!(
            err,
            CommandError::Unknown {
                token: "zz".to_owned()
            }
        );
        assert_eq!(
            err.to_string(),
            "unknown command `:zz` (type :help for the list of commands)"
        );
        assert_eq!(parse(":"), Err(CommandError::MissingName));
        assert_eq!(parse(": build"), Err(CommandError::MissingName));
        assert_eq!(parse(":123"), Err(CommandError::MissingName));
    }

    #[test]
    fn test_shell() {
        let c = parse(":!ls -la").unwrap().unwrap();
        assert_eq!(c.spec.id, CommandId::Shell);
        assert_eq!(c.token, "!");
        assert_eq!(c.arg, "ls -la");
        let c = parse(":shell echo 'a  b'").unwrap().unwrap();
        assert_eq!(c.spec.id, CommandId::Shell);
        assert_eq!(c.arg, "echo 'a  b'");
        assert!(matches!(
            parse(":!").unwrap_err(),
            CommandError::MissingArgument { .. }
        ));
    }

    #[test]
    fn test_arguments() {
        assert_eq!(
            parse(":quit now").unwrap_err().to_string(),
            "`:quit` takes no argument"
        );
        assert_eq!(parse(":reset").unwrap().unwrap().arg, "");
        assert_eq!(
            parse(":b").unwrap_err().to_string(),
            "`:build` needs an argument; usage: :build <pattern>..."
        );
        assert_eq!(parse(":help").unwrap().unwrap().arg, "");
        // Continuations are joined.
        let c = parse(":b //a \\\n  //b").unwrap().unwrap();
        assert_eq!(c.arg, "//a    //b");
        assert_eq!(split_args(&c.arg).unwrap(), vec!["//a", "//b"]);
        // Queries are passed raw.
        let c = parse(":cq deps('//a:b', 1) + \"x y\"").unwrap().unwrap();
        assert_eq!(c.arg, "deps('//a:b', 1) + \"x y\"");
    }

    #[test]
    fn test_split_command_token() {
        assert_eq!(
            split_command_token("  :bu //x"),
            Some(CommandToken {
                token: "bu",
                token_start: 3,
                arg_start: 5
            })
        );
        assert_eq!(
            split_command_token(":"),
            Some(CommandToken {
                token: "",
                token_start: 1,
                arg_start: 1
            })
        );
        assert_eq!(split_command_token("x"), None);
        assert_eq!(split_command_token(":é").map(|t| t.token), Some(""));
    }

    #[test]
    fn test_split_args() {
        assert_eq!(
            split_args("//a 'b c' \"d\\\"e\"").unwrap(),
            vec!["//a", "b c", "d\"e"]
        );
        assert_eq!(split_args("").unwrap(), Vec::<String>::new());
        assert_eq!(split_args("'open"), Err(ArgError::Quoting));
        assert_eq!(
            split_dashdash(vec!["a".to_owned(), "--".to_owned(), "--".to_owned()]),
            (vec!["a".to_owned()], vec!["--".to_owned()])
        );
    }

    #[test]
    fn test_run_args() {
        assert_eq!(
            parse_run_args("//:greet -- a 'b c'").unwrap(),
            RunArgs {
                print: false,
                target: "//:greet".to_owned(),
                args: vec!["a".to_owned(), "b c".to_owned()],
            }
        );
        assert_eq!(
            parse_run_args("--print :greet").unwrap(),
            RunArgs {
                print: true,
                target: ":greet".to_owned(),
                args: vec![],
            }
        );
        assert_eq!(parse_run_args("--print"), Err(ArgError::MissingTarget));
        assert_eq!(
            parse_run_args("//:a //:b"),
            Err(ArgError::ExtraArgument("//:b".to_owned()))
        );
        assert_eq!(
            parse_run_args("-x //:a"),
            Err(ArgError::UnknownFlag("-x".to_owned()))
        );
    }

    #[test]
    fn test_bxl_args() {
        assert_eq!(
            parse_bxl_args("//pkg:x.bxl:main").unwrap(),
            BxlArgs {
                label: "//pkg:x.bxl:main".to_owned(),
                args: Vec::new(),
            }
        );
        assert_eq!(
            parse_bxl_args("x.bxl:main -- --name 'a b' -- c").unwrap(),
            BxlArgs {
                label: "x.bxl:main".to_owned(),
                args: vec![
                    "--name".to_owned(),
                    "a b".to_owned(),
                    "--".to_owned(),
                    "c".to_owned()
                ],
            }
        );
        assert_eq!(
            parse_bxl_args("x.bxl:main --name a"),
            Err(ArgError::ExtraBxlArgument("--name".to_owned()))
        );
        assert_eq!(
            parse_bxl_args("--x x.bxl:main"),
            Err(ArgError::UnknownFlag("--x".to_owned()))
        );
        assert_eq!(
            parse_bxl_args("-- --name a"),
            Err(ArgError::MissingBxlFunction)
        );
        assert_eq!(parse_bxl_args("'x.bxl:main"), Err(ArgError::Quoting));
        assert!(
            ArgError::ExtraBxlArgument("a".to_owned())
                .to_string()
                .contains("after `--`")
        );
    }

    #[test]
    fn test_split_bxl_function() {
        assert_eq!(
            split_bxl_function("//pkg:x.bxl:ma"),
            Some(("//pkg:x.bxl", "ma"))
        );
        assert_eq!(split_bxl_function("x.bxl:"), Some(("x.bxl", "")));
        assert_eq!(
            split_bxl_function("cell//a/x.bxl:m"),
            Some(("cell//a/x.bxl", "m"))
        );
        assert_eq!(split_bxl_function("//pkg:x.b"), None);
        assert_eq!(split_bxl_function("//pkg:x.bxl"), None);
        assert_eq!(split_bxl_function("sub/x.bxl"), None);
        assert_eq!(split_bxl_function(""), None);
    }

    #[test]
    fn test_hidden() {
        assert_eq!(parsed_id(":__locate //:x"), CommandId::Locate);
        assert_eq!(parsed_id(":__edited /a/b.bzl"), CommandId::Edited);
        assert!(command(CommandId::Locate).unwrap().hidden);
        // `:e` is `:edit`, `:i` is `:info`, not a prefix of a hidden command.
        assert_eq!(parsed_id(":e"), CommandId::Edit);
        assert_eq!(parsed_id(":i //:x"), CommandId::Info);
        assert_eq!(parsed_id(":ed"), CommandId::Edit);
        assert_eq!(parsed_id(":hist 3"), CommandId::Hist);
        assert_eq!(parsed_id(":history"), CommandId::Hist);
        assert_eq!(parsed_id(":qd"), CommandId::Qdoc);
    }

    #[test]
    fn test_count() {
        assert_eq!(parse_count(""), Ok(None));
        assert_eq!(parse_count(" 12 "), Ok(Some(12)));
        assert_eq!(parse_count("x"), Err(ArgError::NotACount("x".to_owned())));
        assert_eq!(parse_count("-1"), Err(ArgError::NotACount("-1".to_owned())));
    }

    #[test]
    fn test_set_args() {
        assert_eq!(
            parse_set_args("").unwrap(),
            SetArgs {
                key: None,
                value: None
            }
        );
        let args = parse_set_args("color").unwrap();
        assert_eq!(args.key.map(|s| s.name), Some("color"));
        assert_eq!(args.value, None);
        let args = parse_set_args("target_platforms //p:x").unwrap();
        assert_eq!(args.key.map(|s| s.side), Some(SettingSide::Server));
        assert_eq!(args.value, Some(vec!["//p:x".to_owned()]));
        assert_eq!(
            parse_set_args("modifiers ''").unwrap().value,
            Some(vec![String::new()])
        );
        assert_eq!(
            parse_set_args("modifiers a b").unwrap().value,
            Some(vec!["a".to_owned(), "b".to_owned()])
        );
        let err = parse_set_args("nope 1").unwrap_err();
        assert_eq!(err, ArgError::UnknownSetting("nope".to_owned()));
        assert!(
            err.to_string()
                .contains("target_platforms, modifiers, color")
        );
        assert_eq!(parse_set_args("color 'x"), Err(ArgError::Quoting));
        assert_eq!(
            setting_line("color", "on"),
            format!("color{}on", " ".repeat(18))
        );
        // Every setting has a unique name.
        let names: HashSet<&str> = SETTINGS.iter().map(|s| s.name).collect();
        assert_eq!(names.len(), SETTINGS.len());
        assert!(SETTINGS.iter().all(|s| s.name.len() < SETTING_NAME_WIDTH));
    }

    #[test]
    fn test_load_args() {
        assert_eq!(
            parse_load_args("//pkg:helpers.bxl double triple").unwrap(),
            LoadArgs {
                module: "//pkg:helpers.bxl".to_owned(),
                symbols: vec!["double".to_owned(), "triple".to_owned()],
            }
        );
        assert_eq!(
            parse_load_args("x.bzl").unwrap(),
            LoadArgs {
                module: "x.bzl".to_owned(),
                symbols: vec![],
            }
        );
        assert_eq!(parse_load_args(""), Err(ArgError::MissingTarget));
    }

    #[test]
    fn test_query_dialect() {
        assert_eq!(QueryDialect::Cquery.ctx_method(), "cquery");
        assert_eq!(
            lookup_exact("aq").map(|c| c.arg),
            Some(ArgKind::Query(QueryDialect::Aquery))
        );
    }
}
