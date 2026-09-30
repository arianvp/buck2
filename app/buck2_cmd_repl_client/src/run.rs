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
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Instant;

use buck2_cli_proto::ReplRun;
use buck2_wrapper_common::BUCK_WRAPPER_START_TIME_ENV_VAR;
use buck2_wrapper_common::BUCK_WRAPPER_UUID_ENV_VAR;
use buck2_wrapper_common::BUCK2_WRAPPER_ENV_VAR;

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
    render::print_note(style, &format!("[{how} in {:.2}s]", elapsed.as_secs_f64()))?;
    Ok(rendered)
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
    ui.set(UiState::Child);
    let status = child.status();
    ui.set(after);
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
    render::print_note(style, &format!("[{how}]"))?;
    Ok(rendered)
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

/// The editor of `:edit`: `$VISUAL`, else `$EDITOR`, else `vi` (`notepad` on Windows). It is a
/// command line for the shell (e.g. `code --wait`).
pub(crate) fn editor() -> String {
    // `VISUAL` and `EDITOR` are ambient conventions, not buck2 settings.
    ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|e| !e.trim().is_empty()))
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "notepad".to_owned()
            } else {
                "vi".to_owned()
            }
        })
}

/// Whether the editor command takes `+<line>` (by the name of its program).
fn takes_line(editor: &str) -> bool {
    let program = editor.split_whitespace().next().unwrap_or("");
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    LINE_EDITORS.contains(&name)
}

/// Runs the editor of `:edit` on `path` (at `line`, if the editor is known to take one) with the
/// terminal, and waits for it. `after` is the state of the terminal once it has ended. The
/// error is a message (without `error: `).
pub(crate) fn run_editor(
    path: &Path,
    line: Option<usize>,
    env: &RunEnv,
    ui: &SharedUi,
    after: UiState,
) -> Result<(), String> {
    render::flush().map_err(|e| format!("{e}"))?;
    let editor = editor();
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
    ui.set(UiState::Child);
    let status = command.status();
    ui.set(after);
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!(
            "the editor (`{editor}`) {}; nothing was loaded or run",
            describe(status).0
        )),
        Err(e) => Err(format!("cannot run the editor `{editor}`: {e}")),
    }
}

/// `exited N` or `killed by signal S`, and what it means for the input.
fn describe(status: ExitStatus) -> (String, Rendered) {
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
