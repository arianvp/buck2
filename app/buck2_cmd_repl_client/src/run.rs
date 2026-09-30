/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! `:run`: the daemon builds the target and sends its command line; the client runs it as a
//! child process with the terminal (stdin, stdout, stderr) of the session, then prints how it
//! ended. Also `:!` (a shell command) and the editor of `:edit`.
//!
//! The environment is the client's, as for `buck2 run`: `BUCK_RUN_BUILD_ID` is set and the
//! variables of the buck2 wrapper are removed. While the child runs, SIGINT (Ctrl-C) is for the
//! child: the terminal sends it to the whole foreground process group, and the session ignores
//! it.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Instant;

use buck2_cli_proto::ReplRun;
use buck2_cli_proto::repl_output;
use buck2_util::threads::thread_spawn_scoped;
use buck2_wrapper_common::BUCK_WRAPPER_START_TIME_ENV_VAR;
use buck2_wrapper_common::BUCK_WRAPPER_UUID_ENV_VAR;
use buck2_wrapper_common::BUCK2_WRAPPER_ENV_VAR;

use crate::json::JsonCapture;
use crate::render;
use crate::render::Rendered;
use crate::render::Style;
use crate::session::SharedUi;
use crate::session::UiState;

/// Variables of the buck2 wrapper, which must not leak into the program (as in `buck2 run`).
const WRAPPER_ENV_VARS: [&str; 3] = [
    BUCK2_WRAPPER_ENV_VAR,
    BUCK_WRAPPER_UUID_ENV_VAR,
    BUCK_WRAPPER_START_TIME_ENV_VAR,
];

/// What the programs of `:run` get from the session.
#[derive(Clone, Debug)]
pub(crate) struct RunEnv {
    /// The trace id of the session, as `BUCK_RUN_BUILD_ID`.
    pub(crate) trace_id: String,
    /// The client's working directory, where a program runs unless the daemon says otherwise.
    pub(crate) cwd: PathBuf,
    /// The program reads the terminal. Otherwise (non-interactive mode) its stdin is empty: the
    /// session's stdin holds the next inputs.
    pub(crate) interactive: bool,
}

/// Runs the program of `:run` to its end, then prints `[exited N in Xs]` (or `[killed by
/// signal S in Xs]`). `after` is the state of the terminal once it has ended.
///
/// The input succeeded if the program exited with 0; it was interrupted if SIGINT killed it.
/// Fails only if the output cannot be written.
pub(crate) fn run_program(
    run: &ReplRun,
    env: &RunEnv,
    ui: &SharedUi,
    after: UiState,
    style: Style,
) -> buck2_error::Result<Rendered> {
    let Some((program, args)) = run.argv.split_first() else {
        render::print_error(
            style,
            &format!("error: the command line of `{}` is empty", run.label),
        )?;
        return Ok(Rendered::Failed);
    };
    // What the session printed so far comes before what the program prints.
    render::flush()?;

    // Not `background_command`: on Windows, it would hide the program's console window.
    // ast-grep-ignore: rust/buck2-no-command-new
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .current_dir(if run.cwd.is_empty() {
            env.cwd.clone()
        } else {
            PathBuf::from(&run.cwd)
        })
        .env("BUCK_RUN_BUILD_ID", &env.trace_id);
    for var in WRAPPER_ENV_VARS {
        command.env_remove(var);
    }
    if !env.interactive {
        command.stdin(Stdio::null());
    }

    ui.set(UiState::Child);
    let start = Instant::now();
    let status = command.status();
    let elapsed = Instant::now() - start;
    ui.set(after);

    let status = match status {
        Ok(status) => status,
        Err(e) => {
            render::print_error(style, &format!("error: cannot run `{program}`: {e}"))?;
            return Ok(Rendered::Failed);
        }
    };
    let (how, rendered) = describe(status);
    let newline = after_echoed_interrupt(&rendered);
    render::print_note(
        style,
        &format!("{newline}[{how} in {:.2}s]", elapsed.as_secs_f64()),
    )?;
    Ok(rendered)
}

/// A newline to end the line of the `^C` that the terminal echoed when Ctrl-C interrupted a
/// program that has it (which a note would be written after).
fn after_echoed_interrupt(rendered: &Rendered) -> &'static str {
    if matches!(rendered, Rendered::Interrupted) && std::io::stderr().is_terminal() {
        "\n"
    } else {
        ""
    }
}

/// `:!<command>`: runs the command with the user's shell (`$SHELL -c`, or `/bin/sh`; `cmd /C`
/// on Windows) in the client's working directory, with the terminal of the session (stdin only
/// interactively, as for `:run`). Prints `[exited N]` (or `[killed by signal S]`) unless it
/// exited with 0. `after` is the state of the terminal once it has ended.
///
/// Fails only if the output cannot be written.
pub(crate) fn run_shell(
    command: &str,
    env: &RunEnv,
    ui: &SharedUi,
    after: UiState,
    style: Style,
) -> buck2_error::Result<Rendered> {
    // What the session printed so far comes before what the command prints.
    render::flush()?;
    let (shell, mut child) = shell_command(command, env);
    let status = run_child(&mut child, ui, after, None);
    let status = match status {
        Ok(status) => status,
        Err(e) => {
            render::print_error(style, &format!("error: cannot run `{shell}`: {e}"))?;
            return Ok(Rendered::Failed);
        }
    };
    if status.success() {
        return Ok(Rendered::Ok);
    }
    let (how, rendered) = describe(status);
    let newline = after_echoed_interrupt(&rendered);
    render::print_note(style, &format!("{newline}[{how}]"))?;
    Ok(rendered)
}

/// `:!<command>` with `--json`: runs the command as [`run_shell`] does (stdin is empty), and
/// captures what it writes. The error is a message (without `error: `).
pub(crate) fn run_shell_captured(
    command: &str,
    env: &RunEnv,
    ui: &SharedUi,
    after: UiState,
    capture: &JsonCapture,
) -> Result<ExitStatus, String> {
    let (shell, mut child) = shell_command(command, env);
    run_child(&mut child, ui, after, Some(capture))
        .map_err(|e| format!("cannot run `{shell}`: {e}"))
}

/// The command that runs `command` with the shell (see [`run_shell`]), and the shell.
fn shell_command(command: &str, env: &RunEnv) -> (String, std::process::Command) {
    let (shell, flag) = shell();
    // ast-grep-ignore: rust/buck2-no-command-new
    let mut child = std::process::Command::new(&shell);
    child.arg(flag).arg(command).current_dir(&env.cwd);
    for var in WRAPPER_ENV_VARS {
        child.env_remove(var);
    }
    if !env.interactive {
        child.stdin(Stdio::null());
    }
    (shell, child)
}

/// Runs a program of the session (`:run`, `:!`, the editor of `:edit`) to its end, with the
/// terminal (SIGINT is for it meanwhile), or with its output captured for the input's JSON
/// record. `after` is the state of the terminal once it has ended.
fn run_child(
    command: &mut std::process::Command,
    ui: &SharedUi,
    after: UiState,
    capture: Option<&JsonCapture>,
) -> std::io::Result<ExitStatus> {
    ui.set(UiState::Child);
    let result = match capture {
        None => command.status(),
        Some(capture) => run_captured(command, capture),
    };
    ui.set(after);
    result
}

/// Runs a program with an empty stdin and what it writes captured into `capture`, as it
/// writes it: past what the capture keeps, the output is read and dropped, so that a program
/// that writes without end (`:!yes`) does not fill the client's memory.
fn run_captured(
    command: &mut std::process::Command,
    capture: &JsonCapture,
) -> std::io::Result<ExitStatus> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // Both pipes are read at once: a program may fill one while the other is read.
    let drained = std::thread::scope(|scope| {
        if let Some(stderr) = stderr {
            thread_spawn_scoped("repl-capture", scope, move || {
                drain(stderr, capture, repl_output::Channel::Stderr)
            })?;
        }
        if let Some(stdout) = stdout {
            drain(stdout, capture, repl_output::Channel::Stdout);
        }
        Ok::<(), std::io::Error>(())
    });
    if let Err(e) = drained {
        let _ignored = child.kill();
        let _ignored = child.wait();
        return Err(e);
    }
    child.wait()
}

/// Reads `pipe` to its end into `capture`.
fn drain(mut pipe: impl Read, capture: &JsonCapture, channel: repl_output::Channel) {
    let mut buf = vec![0; 64 << 10];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => capture.write(channel, buf.get(..n).unwrap_or_default()),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            // The program's output cannot be read: what it writes next is lost.
            Err(_) => return,
        }
    }
}

/// The shell of `:!`, and its flag to run a command.
fn shell() -> (String, &'static str) {
    if cfg!(windows) {
        ("cmd".to_owned(), "/C")
    } else {
        // `SHELL` is the user's shell, an ambient convention, not a buck2 setting.
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".to_owned());
        (shell, "-c")
    }
}

/// Editors known to take `+<line>` before a file, to open it at that line.
const LINE_EDITORS: &[&str] = &[
    "vi",
    "vim",
    "nvim",
    "gvim",
    "mvim",
    "view",
    "nano",
    "pico",
    "emacs",
    "emacsclient",
    "micro",
    "kak",
    "mg",
    "joe",
    "jed",
    "ne",
];

/// The editor the user chose for `:edit`: `$VISUAL`, else `$EDITOR`. It is a command line for
/// the shell (e.g. `code --wait`).
fn chosen_editor() -> Option<String> {
    // `VISUAL` and `EDITOR` are ambient conventions, not buck2 settings.
    ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|e| !e.trim().is_empty()))
}

/// The editor of `:edit` when the user chose none.
const DEFAULT_EDITOR: &str = if cfg!(windows) { "notepad" } else { "vi" };

/// Whether the editor command takes `+<line>` (by the name of its program).
fn takes_line(editor: &str) -> bool {
    let program = editor.split_whitespace().next().unwrap_or("");
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    LINE_EDITORS.contains(&name)
}

/// Runs the editor of `:edit` on `path` (at `line`, if the editor is known to take one) with the
/// terminal (or, with `--json`, with what it writes captured), and waits for it. `after` is the
/// state of the terminal once it has ended. The error is a message (without `error: `).
pub(crate) fn run_editor(
    path: &Path,
    line: Option<usize>,
    env: &RunEnv,
    ui: &SharedUi,
    after: UiState,
    capture: Option<&JsonCapture>,
) -> Result<(), String> {
    let editor = match chosen_editor() {
        Some(editor) => editor,
        // A terminal editor without the terminal (its stdin is empty) may never end, and the
        // session would wait for it.
        None if !env.interactive => {
            return Err(format!(
                "`:edit` needs VISUAL or EDITOR set when the session does not read a terminal \
                 (the default editor, `{DEFAULT_EDITOR}`, needs one); nothing was loaded or run"
            ));
        }
        None => DEFAULT_EDITOR.to_owned(),
    };
    render::flush().map_err(|e| format!("{e}"))?;
    let mut args: Vec<OsString> = Vec::new();
    if let Some(line) = line
        && takes_line(&editor)
    {
        args.push(format!("+{line}").into());
    }
    args.push(path.as_os_str().to_owned());
    let mut command = if cfg!(windows) {
        // ast-grep-ignore: rust/buck2-no-command-new
        let mut command = std::process::Command::new("cmd");
        command.arg("/C").arg(&editor);
        command
    } else {
        // The editor is a command line (`code --wait`): the shell splits it, and the arguments
        // are passed as they are (as git runs its editor).
        // ast-grep-ignore: rust/buck2-no-command-new
        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(format!("{editor} \"$@\""))
            .arg(&editor);
        command
    };
    command.args(&args).current_dir(&env.cwd);
    if !env.interactive {
        command.stdin(Stdio::null());
    }
    match run_child(&mut command, ui, after, capture) {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!(
            "the editor (`{editor}`) {}; nothing was loaded or run",
            describe(status).0
        )),
        Err(e) => Err(format!("cannot run the editor `{editor}`: {e}")),
    }
}

/// `exited N` or `killed by signal S`, and what it means for the input.
pub(crate) fn describe(status: ExitStatus) -> (String, Rendered) {
    if let Some(code) = status.code() {
        let rendered = if code == 0 {
            Rendered::Ok
        } else {
            Rendered::Failed
        };
        return (format!("exited {code}"), rendered);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        /// SIGINT: the program was interrupted with Ctrl-C.
        const SIGINT: i32 = 2;

        if let Some(signal) = status.signal() {
            let rendered = if signal == SIGINT {
                Rendered::Interrupted
            } else {
                Rendered::Failed
            };
            return (format!("killed by signal {signal}"), rendered);
        }
    }
    (format!("ended with {status}"), Rendered::Failed)
}
