---
id: repl
title: Exploring the build interactively with buck2 repl
---

# Exploring the build interactively with `buck2 repl`

`buck2 repl` starts an interactive BXL/Starlark session with the daemon. Anything
you type is evaluated as a line of a BXL function with `ctx` (a
[`bxl.Context`](../../../api/bxl/Context)) bound, so you can query the target
graph, look at providers, build targets, run binaries and try out code from your
`.bxl` and `.bzl` files, without writing and re-running a script for each
question. Everything you compute stays bound for the next input.

```text
$ cd foo && buck2 repl
buck2 repl · root//foo · ctx is a bxl.Context · :help for commands · Ctrl-D to exit
root//foo> t = ctx.configured_targets(":server")
root//foo> t.rule_type
"root//rules/defs.bzl:cxx_binary"
root//foo> :cq deps(:server, 1)
[
  root//foo:server (cfg:linux-x86_64#a1b2),
  root//foo:lib (cfg:linux-x86_64#a1b2)
]
root//foo> len(_)
2
root//foo> :b :server
root//foo:server  buck-out/v2/art/root/9f8e7d6c5b4a3210/foo/__server__/server
root//foo> :run :server -- --port 8080
listening on :8080
[exited 0 in 3.21s]
```

The same session can be driven by a program or a script (`-e`, stdin, `--json`):
see [Scripting](#scripting).

## Starting a session

Run `buck2 repl` anywhere in a project. The session's directory is the one you
start it in: the prompt shows it as a package (`root//foo> `), and the targets,
packages, queries and modules of the session are relative to it (see
[Patterns are relative](#patterns-are-relative-to-the-sessions-directory)); `ctx.fs`
takes paths relative to the project root, as in `buck2 bxl`.

Useful flags (`buck2 help repl` lists them all):

| Flag                                                 | Meaning                                                                                       |
| ---------------------------------------------------- | --------------------------------------------------------------------------------------------- |
| `FILES...`                                           | Load or evaluate these files first (see [Preloading files](#preloading-files))                |
| `-e INPUT`                                           | Evaluate `INPUT` and exit (repeatable)                                                        |
| `-i`                                                 | After the files and `-e` inputs, go on with the prompt (or stdin)                             |
| `--json`                                             | One JSON object per input on stdout (non-interactive only)                                    |
| `--continue-on-error`                                | Non-interactive: keep going after a failing input                                             |
| `--target-platforms`, `-m`                           | The target platform and modifiers that configure targets (can be changed later with `:set`)   |
| `--prefer-local`, `--keep-going`, `--fail-fast`, ... | Build options, for every build of the session (`:build`, `:run`, `:bxl`, `ctx.output.ensure`) |
| `--max-heap-mb MIB`                                  | Fail inputs once the session's values use more memory than this (default 4096; see below)     |
| `--no-history`                                       | Do not read or write the history file                                                         |
| `--console`, `--ui`                                  | How the progress of an input is shown (see [Progress](#progress-while-an-input-runs))         |

Build options, `-c`/`--config` and the other buck2 flags are fixed when the
session starts; start a new session to change them. The session writes no build
report (`--build-report` and `--streaming-build-report` are refused): `:build`
lists what it built.

End the session with Ctrl-D on an empty line or `:quit` (`:q`).

## Evaluating code

Type Starlark. An input's value is printed (unless it is `None`) and becomes `_`;
assignments, `def`s and `load`s print nothing:

```text
root//> x = 41
root//> x + 1
42
root//> _ * 2
84
root//> def owners(path):
    return ctx.uquery().owner(path)

root//> owners("foo/main.cpp")
[ root//foo:server ]
```

Everything BXL offers is there: `ctx.cquery()`, `ctx.uquery()`, `ctx.aquery()`,
`ctx.analysis(...)`, `ctx.configured_targets(...)`, `ctx.unconfigured_targets(...)`,
`ctx.target_universe(...)`, `ctx.fs`, `ctx.build(...)`, `ctx.output`, ... Values
are pretty-printed, one element per line when there are several; target nodes,
contexts and ensured artifacts are shown as summaries (`ctx` itself is shown as
`<bxl.Context cwd=root//foo>`).

- `print(...)` writes to stdout as the input runs.
- `ctx.output.print(...)` and `ctx.output.print_json(...)` write to stdout once the
  input is done, `ctx.output.stream(...)` at once, as in `buck2 bxl`.
- `ctx.output.ensure(artifact)` materializes the artifact once the input is done;
  its value is printed as the artifact's path.

```text
root//> a = ctx.analysis(ctx.configured_targets("//:hello")).providers()[DefaultInfo].default_outputs[0]
root//> ctx.output.ensure(a)
buck-out/v2/art/root/1ef78538d8598cb2/__hello__/hello.txt
```

`ctx.bxl_actions()` is not available at the prompt: actions must be declared by a
function of a real `.bxl` file. Use `:bxl` for that (see
[BXL functions](#running-bxl-functions)).

### Multi-line inputs

Enter submits the input when it is complete; otherwise it starts a new line
(there is no second prompt: the input is one buffer that you can edit as a whole).
An input is incomplete while a bracket or a triple-quoted string is open, when a
line ends with `\` or `:`, and while a block (`def`, `for`, `if`, ...) has not
been ended by an empty line, as in Python. Alt-Enter (or Esc Enter) always starts
a new line; Tab at the start of a line indents by 4 spaces. Pasted code may be
indented: the common indentation is removed. A command (`:...`) is one line,
unless a line ends with `\` (the lines are joined).

### Long values, errors and interrupting

- At the prompt, a value is cut after 40 lines (`… N more lines (:p _ to show all)`);
  `:print _` prints all of it (up to 16 MiB). Every echoed value is cut at 64 KiB, a
  value nested too deeply to format safely is shown as
  `<value nested too deeply to display>`, and an int of more than 262144 bits (about
  79000 digits) as `<int of N bits: too large to display>` (`str(x)` still converts
  it).
- Errors are shown with the input, named `<repl:N>` (the input's number, see
  `:hist`), and the session goes on.
- Ctrl-C interrupts the input that runs (`^C interrupting…`): Starlark code stops
  at its next step, and a build or a query is cancelled. An input can take a while
  to stop: it may be waiting for another buck2 command that holds the daemon, or
  be inside one native operation that cannot be interrupted (comparing or hashing
  large structures that share their parts, `all`, `max` or `sorted` over a huge
  range, `str` of a huge value, ...). A second Ctrl-C says so, and a third one ends
  the session. An operation that cannot be interrupted goes on in the daemon until
  it ends, and other buck2 commands wait for it; `buck2 kill` stops it.
- When an input takes a second or more, its duration is shown after it (`(1.24s)`,
  with the time spent waiting for the daemon if that was long). `:time <input>`
  always shows it.
- `note: files or settings changed since the previous input (here or for another buck2 command); values computed earlier may be stale`
  means that the daemon's state changed since the previous input: files changed,
  or another buck2 command ran with other settings (`-c`, ...), which the daemon
  does not tell apart. After a change of files, values bound earlier (nodes,
  analysis results, artifacts) describe the old sources: compute them again, and
  `:reload` the modules you loaded.
- The values of a session are never freed until it ends or `:reset`s. Once they
  use more than the `--max-heap-mb` limit (4 GiB by default), inputs fail until
  `:reset`. The limit is checked between the steps of an input, as Starlark does
  everywhere: one operation (`"a" * 1000000000`, `list(range(10000000000))`) can
  allocate any amount before it is checked, even more than the machine has.

## Commands

A line that starts with `:` is a command. A command can be shortened to any unique
prefix (`:prov` for `:providers`), and most have aliases.

| Command                                 | Alias      | What it does                                                                        |
| --------------------------------------- | ---------- | ----------------------------------------------------------------------------------- |
| `:help [command\|keys\|patterns]`       | `:h`, `:?` | The commands, or help on one command or topic                                       |
| `:quit`                                 | `:q`       | End the session (as Ctrl-D)                                                         |
| `:time <input>`                         |            | Run an input, then print how long it took (and, for the daemon's inputs, the parts) |
| `:type <expr>`                          | `:t`       | The type of an expression                                                           |
| `:print <expr>`                         | `:p`       | The whole value of an expression (up to 16 MiB); binds `_`                          |
| `:json <expr>`                          | `:j`       | An expression as pretty JSON; binds `_`                                             |
| `:doc <expr>`                           | `:d`       | The documentation of a value (a function, a type, `ctx.cquery`) or of its type      |
| `:load <label> [symbol...]`             | `:l`       | Load symbols of a `.bzl` or `.bxl` file (every public one if none are named)        |
| `:reload`                               | `:r`       | Load the modules loaded so far again, picking up changes                            |
| `:reset`                                |            | Start over: drop every binding and loaded module                                    |
| `:uquery <query>`                       | `:uq`      | Run an unconfigured query; binds `_`                                                |
| `:cquery <query>`                       | `:cq`      | Run a configured query; binds `_`                                                   |
| `:aquery <query>`                       | `:aq`      | Run an action query; binds `_`                                                      |
| `:providers <target>`                   | `:pv`      | Analyze a target and show its providers; binds `_`                                  |
| `:build <pattern>...`                   | `:b`       | Build targets and print their outputs; binds `_`                                    |
| `:run [--print] <target> [-- args...]`  |            | Build a target and run it                                                           |
| `:bxl <file.bxl:function> [-- args...]` |            | Run a BXL function, with actions and outputs                                        |
| `:info <target>`                        | `:i`       | A target's rule type, build file, attributes and deps                               |
| `:ls [package]`                         |            | The targets of a package, with their rule types                                     |
| `:set [key [value...]]`                 |            | Show or change the session's settings                                               |
| `:who [glob...]`                        | `:vars`    | The session's bindings, with their types and values                                 |
| `:hist [n]`                             | `:history` | The inputs of the session, numbered like `<repl:N>`                                 |
| `:!<command>`                           | `:shell`   | Run a shell command in the current directory                                        |
| `:edit [path\|target]`                  | `:e`       | Edit a file, a target's build file, or a scratch buffer to run                      |
| `:qdoc [function\|language]`            |            | The documentation of the query functions                                            |

`:help <command>` explains each one in detail.

### Queries

`:uq`, `:cq` and `:aq` take a query as `buck2 uquery`, `cquery` and `aquery` do
(without quotes; a query quoted whole as on the shell, `:cq 'deps(:lib)'`, is
unquoted), and bind its result to `_`, so you can go on from there:

```text
root//foo> :cq rdeps(//..., :lib, 1)
[
  root//foo:lib (cfg:linux-x86_64#a1b2),
  root//foo:server (cfg:linux-x86_64#a1b2)
]
root//foo> [t.label for t in _ if t.rule_type.endswith("cxx_binary")]
[ root//foo:server (cfg:linux-x86_64#a1b2) ]
root//foo> ctx.cquery().deps(ctx.configured_targets(":server"), 1)
```

`:qdoc` lists the query functions of the three languages, and `:qdoc rdeps` shows
one.

### Types and documentation

`:type` (`:t`) shows the type of a value as the type checker sees it, and what
`type()` returns when that differs. A function, a lambda or a method (such as
`ctx.configured_targets`) shows its signature, with default values shown as
`...`; a value whose signature is not known, such as a `partial`, is just
`function`:

```text
root//> :t "a".join
def(_: typing.Iterable[str], /) -> str  # type() is "function"
root//> :t partial(len)
function
```

`:doc` (`:d`) shows the documentation of a function, a type or a namespace (for
any other value, of its type), and `:qdoc rdeps` that of a query function. The
documentation is written in Markdown and shown as text: headings are bold, code
is highlighted and indented, and paragraphs are wrapped (plain text, without
colours, when colour is off, in scripts and with `--json`):

```text
root//> :doc ctx.cquery().deps
ctx.cquery().deps

    def ctx.cquery().deps(
        universe: ConfiguredTargetLabel | TargetLabel | ... | str],
        depth: None | int = None,
        filter: None | str = None,
    ) -> target_set

The deps query for finding the transitive closure of dependencies.
...
```

### Inspecting targets

```text
root//> :info :hello
root//:hello
  rule        root//defs.bzl:write_file
  build file  TARGETS.fixture:12
  attributes  (those not left to their defaults)
    content     = "hello\n"
    name        = "hello"
    out         = "hello.txt"
    visibility  = []
    within_view = ["PUBLIC"]
  deps        0
root//> :ls
root//:greet  runnable
root//:hello  write_file
root//> :pv :hello
Providers([ DefaultInfo(...) ])
root//> _[DefaultInfo].default_outputs
[ <build artifact hello.txt bound to root//:hello (<unspecified>)> ]
```

### Building and running

`:build` builds targets as `buck2 build` does (with the session's build options),
materializes their outputs and prints one `label  path` line per output; `_`
becomes a dict of the output paths by label. `:run` builds one target and runs its
`RunInfo` command in the current directory, with the terminal (Ctrl-C goes to the
program), then prints how it ended; `--print` prints the command instead.

```text
root//> :b //:hello
root//:hello  buck-out/v2/art/root/1ef78538d8598cb2/__hello__/hello.txt
root//> :run //:greet -- a b
hello from greet a b
[exited 0 in 0.00s]
root//> :run --print //:greet
echo 'hello from greet'
```

Build progress is shown while the input runs, and build errors fail the input.

### Loading code

`load()` works at the prompt, and `:load` is a shorter form that can also import
every public symbol of a module:

```text
root//> :l //tools/helpers.bxl
note: loaded //tools/helpers.bxl: double, main
root//> double(21)
42
```

Modules are labels (`//pkg:defs.bzl`, `@cell//pkg:x.bxl`, `:defs.bzl`) or paths
relative to the session's directory (`defs.bzl`, `sub/x.bxl`, `../x.bzl`). After you change a
loaded file, `:reload` (`:r`) loads every module loaded so far again. `:edit
//tools/helpers.bxl` opens the file in your editor and reloads it when the editor
exits.

### Running BXL functions

At the prompt you evaluate lines of a BXL function, which cannot declare actions.
`:bxl` runs a whole BXL function of a file as `buck2 bxl` does: it may declare and
build actions, its ensured artifacts are materialized, and its output is shown:

```text
root//> :bxl //tools/helpers.bxl:main -- --target //foo:server
hello from main
root//> :bxl tools/helpers.bxl:main -- --help
```

The file is a label or a path relative to the session's directory. The function
runs with the session's target platform; an edit to the file is picked up by the
next `:bxl`. A good way to develop a BXL script is to try its pieces at the prompt
(after `:load`ing its helpers), then run it with `:bxl`.

### Settings

`:set` lists the settings, `:set <key>` shows one, and `:set <key> <value>`
changes it for the rest of the session:

| Setting                            | Meaning                                                                                                          |
| ---------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| `target_platforms <target>\|""`    | The target platform that configures targets (`ctx`, queries, `:build`, `:run`, `:bxl`), as `--target-platforms`  |
| `modifiers <modifier>...\|""`      | The configuration modifiers of every target, as `-m`                                                             |
| `color auto\|on\|off`              | Colour errors, notes and the input as it is typed (auto: interactively, on a terminal, unless `NO_COLOR` is set) |
| `timing auto\|on\|off`             | Show how long inputs take (auto: those that take a second or more; on: every input, as `:time`)                  |
| `completion_timeout_ms <ms>\|auto` | How long Tab waits for the daemon (auto: 500 ms for names, 1000 ms for targets and modules)                      |

`:set target_platforms //platforms:linux_arm` checks that the target is a platform.

### Other commands

- `:who [glob...]` lists what you defined and loaded, with types and the start of
  each value (`:who my_*`).
- `:hist [n]` lists the inputs of the session, numbered as errors name them
  (`<repl:3>`).
- `:!<command>` runs a shell command (`:!git status`, `:!ls buck-out`).
- `:edit` without an argument opens a scratch buffer in your editor (`$VISUAL`,
  `$EDITOR` or `vi`), and evaluates it as one input when the editor exits: handy
  for longer functions. The buffer keeps its text for the next `:edit`. `:edit
  <file>` edits a file, and `:edit <target>` the build file of a target, at its
  line.

## Patterns are relative to the session's directory

Target patterns, packages, queries, modules and the file names in queries are
relative to the session's directory, as on the buck2 command line (in `buck2 bxl`
they are relative to the cell root):

| Pattern         | Means                                                      |
| --------------- | ---------------------------------------------------------- |
| `:lib`          | the target `lib` in the package of the session's directory |
| `sub:lib`       | the target `lib` in the package `sub` below it             |
| `//pkg:lib`     | the target `lib` in the package `pkg` of the current cell  |
| `cell//pkg:lib` | a target in another cell                                   |
| `//pkg:`        | every target in the package `pkg`                          |
| `//pkg/...`     | every target in `pkg` and the packages below it            |
| `...`           | every target at or below the session's directory           |

This holds for every command that takes them (`:build`, `:run`, `:info`, `:ls`,
`:providers`, the queries, `:edit`, `:load`, `:bxl`) and for everything `ctx`
resolves (`ctx.configured_targets(":lib")`, `ctx.cquery().eval("deps(:lib)")`,
`ctx.uquery().owner("main.cpp")`, ...), also inside functions loaded from files. A
relative `load()` is relative to the session's directory, as if the session were
a `.bxl` file there. `ctx.fs` is the exception: it takes paths relative to the
project root, as in `buck2 bxl` (`ctx.fs.exists("foo/BUCK")`).

## Completion

Tab completes what is being typed, asking the daemon when needed:

- commands (`:bu`), `:help` topics and `:set` keys and values;
- names (`dou` → `double(`) and keywords;
- attributes, also after calls whose return type is known
  (`ctx.cquery().de` → `deps(`), and of values bound in the session;
- keyword arguments (`ctx.configured_targets(tar` → `target_platform=`);
- target patterns and subtargets after `:build`, `:run`, `:info`, ... and in
  strings (`"//foo:`, `ctx.configured_targets("`, `"//foo:bar[`);
- query functions, operators and targets after `:cq`, `:uq`, `:aq` and in
  `ctx.cquery().eval("`;
- modules and their symbols in `load("//pkg:` / `load("//pkg:x.bzl", "` and after
  `:load`;
- the files and functions of `:bxl` (`:bxl //tools/helpers.bxl:`);
- paths after `:!`, `:edit` and `:load`.

When there are several candidates, Tab inserts what they share and lists them.
The candidates are the names that start with what you typed; if there are none,
those that do ignoring case; if there are still none, those that contain its
letters in order (`ctx.cfgt` → `configured_targets(`).

While the cursor is in the parentheses of a call of a function that Tab offered,
its signature is shown under the input, with the parameter being typed in bold.

Completion does not evaluate the input (though it loads a module to list its
symbols or BXL functions, as `load()` would), and gives up quickly (see
`completion_timeout_ms`) when the daemon is busy with another command.

## Key bindings

| Key                  | At the prompt                                                  | While an input runs                          |
| -------------------- | -------------------------------------------------------------- | -------------------------------------------- |
| Enter                | Submit the input if it is complete, otherwise start a new line |                                              |
| Alt-Enter, Esc Enter | Start a new line                                               |                                              |
| Tab                  | Indent (at the start of a line), otherwise complete            |                                              |
| Ctrl-C               | Clear the input                                                | Interrupt it; a third press ends the session |
| Ctrl-D               | End the session (on an empty line)                             |                                              |
| Up, Down             | Previous and next input in the history                         |                                              |
| Ctrl-R               | Search the history                                             |                                              |
| Right, End, Ctrl-E   | Accept the suggestion from the history (shown dimmed)          |                                              |
| Ctrl-L               | Clear the screen                                               |                                              |

The input is highlighted as it is typed: keywords, strings, numbers, comments,
the command, and the bracket matching the one at the cursor. `NO_COLOR` or `:set
color off` turns colour off.

The history is kept in `~/.buck/repl_history` (`BUCK2_REPL_HISTORY` sets another
file, an empty value or `--no-history` keeps none).

## Progress while an input runs

On a terminal, an input that runs for a while shows buck2's live progress
(elapsed time, the running actions, `Waiting for command ...` when another command
holds the daemon), which is erased when the input is done: the prompt stays
clean. `--ui` configures it as for other commands (`--ui dice`, `--ui io`, ...).
`--console simple` shows the progress as lines instead (as `buck2 build --console
simple`), `--console super` draws it even when stderr is not a terminal. Scripts
get the simple console.

What the daemon logs while an input runs (the warning of a query that found
nothing, for example) is shown before the input's result. With live progress,
what it logs while you type (for any command: other buck2 commands share the
daemon's log) is held, so that it never garbles the line you are editing, and
shown with the next input after
`note: logged by the daemon (for any command) since the previous input:`.

## Scripting

The session is non-interactive when the inputs come from `-e` or from stdin that is
not a terminal. Then there is no prompt, banner or colour, values are printed as
they are (not cut at 40 lines), a `:run` program gets an empty stdin, and the
session stops at the first failing input, unless `--continue-on-error` is given.

```sh
# Inputs on the command line
buck2 repl -e 'x = ctx.cquery().eval("deps(//foo:server)")' -e 'len(x)'

# A script on stdin: split into inputs as the prompt would split it
buck2 repl < explore.star
printf ':cq rdeps(//..., //foo:lib)\nlen(_)\n' | buck2 repl

# Load helpers, run some inputs, then go on at the prompt
buck2 repl -i tools/helpers.bxl -e 'x = double(21)'
```

Inputs on stdin are split as the prompt splits them, so a block (`def`, `for`,
`if`, ...) is evaluated once a later line ends it. A program that drives the
session through a pipe, waiting for each result, should follow a block with an
empty line.

### Preloading files

`buck2 repl FILES...` evaluates the files before the other inputs, in order:
`.bzl` and `.bxl` files are loaded as `:load` loads them (every public symbol
becomes a binding), other files are evaluated as one Starlark input (without
commands; use stdin for a script with commands). Then come the `-e` inputs, then
stdin if there was no `-e`. `-i` goes on with stdin (the prompt, on a terminal)
after the `-e` inputs too, like Python's `-i`: `buck2 repl -i FILE` loads `FILE`,
then shows the prompt.

### JSON output

With `--json`, each input prints one JSON object on its own line of stdout (JSON
Lines), and nothing else is printed there:

```sh
$ buck2 repl --json -e 'x = 1' -e 'x + 1' -e ':cq deps(//:hello)' -e 'nope'
{"n":1,"input":"x = 1","ok":true,"stdout":"","stderr":"","wait_ms":1,"eval_ms":0,"sources_changed":false}
{"n":2,"input":"x + 1","ok":true,"type":"int","text":"2","json":2,"stdout":"","stderr":"","wait_ms":1,"eval_ms":0,"sources_changed":false}
{"n":3,"input":":cq deps(//:hello)","ok":true,"type":"target_set","text":"[ root//:hello (<unspecified>) ]","stdout":"","stderr":"","wait_ms":1,"eval_ms":2,"sources_changed":false}
{"n":4,"input":"nope","ok":false,"error":{"kind":"eval","message":"error: Variable `nope` not found, ..."},"stdout":"","stderr":"","wait_ms":0,"eval_ms":1,"sources_changed":false}
```

| Field                                   | Meaning                                                                                                                                                         |
| --------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `n`                                     | The input's number (`<repl:N>` in errors), or `null` for an input the client handles alone (`:help`, `:hist`, `:!`, ...)                                        |
| `input`                                 | The input (for a file of the command line: `:load <file>` for a `.bzl` or `.bxl` file, else its code; `file` is its path)                                       |
| `ok`                                    | Whether the input succeeded                                                                                                                                     |
| `type`, `text`                          | The value's type and text, when the input has a value (`truncated` is `true` if the text was cut)                                                               |
| `json`                                  | The value as JSON, when it has a JSON form of at most 1 MiB (as `:json` would show it)                                                                          |
| `run`                                   | For `:run`: the command (`argv`, `label`), which is not run (`stdout` has it, as `:run --print` prints it)                                                      |
| `error`                                 | When `ok` is false: `kind` (`syntax`, `eval`, `buck`, `interrupted`, `usage`, `busy`, `unsupported`, `internal`, `unknown`, `io`, `exit`, `lost`) and `message` |
| `notices`                               | What the daemon said about the input (`{"level": "info", "text": "loaded ..."}`)                                                                                |
| `stdout`, `stderr`                      | What the input wrote (`print()`, `ctx.output`, `:print`, `:help`, a `:!` command, ...); `stdout_truncated`/`stderr_truncated` past 64 MiB                       |
| `wait_ms`, `eval_ms`, `sources_changed` | How long the daemon waited and evaluated, and whether files or settings changed since the previous input, when the daemon answered                              |

`--json` requires non-interactive inputs. An input that is still running when the
session ends gets a record too: its error kind is `interrupted` when the daemon
cancelled it (`buck2 kill`), and `lost` when no answer came (the daemon died).

### Exit codes

| Situation                                                                          | Exit code                                                   |
| ---------------------------------------------------------------------------------- | ----------------------------------------------------------- |
| An interactive session ends (Ctrl-D, `:quit`)                                      | 0, whatever its inputs did                                  |
| Non-interactive, every input succeeded                                             | 0                                                           |
| Non-interactive, an input failed                                                   | 3 (also with `--continue-on-error`)                         |
| An input of a script was interrupted (SIGINT), or a third Ctrl-C ended the session | 141, as other buck2 commands                                |
| The daemon could not be reached, was killed, ...                                   | buck2's usual [exit codes](../commands_extra/exit_codes.md) |

### Completion from a script

`:__complete {"buf": "ctx.cq", "pos": 6}` prints the candidates Tab would offer
for a buffer (at byte offset `pos`, by default its end) as JSON, which editor
integrations and tests can use.

## Good to know

- A session uses the daemon only while an input runs: other buck2 commands run
  between inputs, and an input waits (`Waiting for command ...`) while another
  command holds the daemon. `buck2 status` lists the session. An input that cannot
  be interrupted (see [Long values, errors and interrupting](#long-values-errors-and-interrupting))
  holds the daemon until it ends, even after the session is gone; `buck2 kill` stops it.
- `buck2 kill` or a daemon restart ends the session.
- Environment variables: `BUCK2_REPL_HISTORY` (the history file) and
  `BUCK2_REPL_COMPLETION_TIMEOUT_MS` (how long Tab waits for the daemon).
