/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Rendering of results: values on stdout, errors and notices on stderr.

use std::io::IsTerminal;
use std::time::Duration;

use buck2_cli_proto::ReplDone;
use buck2_cli_proto::ReplNotice;
use buck2_cli_proto::ReplRun;
use buck2_cli_proto::ReplValue;
use buck2_cli_proto::repl_done;
use buck2_cli_proto::repl_error;
use buck2_cli_proto::repl_notice;
use buck2_repl_syntax::text::truncate_lines;

/// Most lines of a value the interactive editor shows.
const MAX_VALUE_LINES: usize = 40;

/// An input that takes this long (waiting for the daemon plus evaluating) shows its duration.
const SHOW_DURATION: Duration = Duration::from_secs(1);

/// Its wait for the daemon is shown too when it is at least this long.
const SHOW_WAIT: Duration = Duration::from_millis(500);

const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// How results are shown.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Style {
    /// For the interactive editor: long values are cut to [`MAX_VALUE_LINES`] lines, and slow
    /// inputs show their duration.
    interactive: bool,
    /// Errors and notes on stderr are coloured.
    color: bool,
    /// Slow inputs show their duration (interactive only). Off for `:time`, which shows more.
    durations: bool,
}

impl Style {
    /// Non-interactive mode: no colour, no line limit, no durations.
    pub(crate) fn script() -> Self {
        Style {
            interactive: false,
            color: false,
            durations: false,
        }
    }

    /// The interactive editor. Colour unless stderr is not a terminal or `NO_COLOR` is set.
    pub(crate) fn interactive() -> Self {
        // `NO_COLOR` is an ambient convention (https://no-color.org), not a buck2 setting.
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        Style {
            interactive: true,
            color: !no_color && std::io::stderr().is_terminal(),
            durations: true,
        }
    }

    /// Without the duration of slow inputs.
    pub(crate) fn without_durations(self) -> Self {
        Style {
            durations: false,
            ..self
        }
    }

    /// `text` in `color`, when colour is on.
    fn paint(&self, color: &str, text: &str) -> String {
        if self.color {
            format!("{color}{text}{RESET}")
        } else {
            text.to_owned()
        }
    }
}

/// How an input went.
pub(crate) enum Rendered {
    Ok,
    Failed,
    Interrupted,
    /// `:run`: the program is to be run (see [`run_program`](crate::run::run_program)).
    Run(ReplRun),
}

/// Prints the result of an input. For `:run` (without `--print`), the caller runs the program.
///
/// Fails if the output cannot be written (e.g. stdout is a closed pipe): the caller stops, since
/// nobody sees the results of later inputs.
pub(crate) fn render_done(done: &ReplDone, style: Style) -> buck2_error::Result<Rendered> {
    if done.sources_changed {
        print_note(
            style,
            "note: sources changed since the previous input; values computed earlier may be stale",
        )?;
    }
    let rendered = match &done.outcome {
        None => Rendered::Ok,
        Some(repl_done::Outcome::Value(value)) => {
            print_value(value, style)?;
            Rendered::Ok
        }
        Some(repl_done::Outcome::Error(error)) => {
            if error.kind() == repl_error::Kind::Interrupted {
                buck2_client_ctx::eprintln!("{}", style.paint(YELLOW, "interrupted"))?;
                Rendered::Interrupted
            } else {
                print_error(style, &error.message)?;
                Rendered::Failed
            }
        }
        Some(repl_done::Outcome::Run(run)) => {
            if run.print_only {
                // `:run --print`: the command line, quoted for a POSIX shell.
                let argv = run.argv.iter().map(String::as_str);
                let command = shlex::try_join(argv).unwrap_or_else(|_| run.argv.join(" "));
                buck2_client_ctx::println!("{}", command)?;
                Rendered::Ok
            } else {
                Rendered::Run(run.clone())
            }
        }
    };
    if style.interactive && style.durations {
        print_duration(done, style)?;
    }
    Ok(rendered)
}

/// What `:time` prints: `time: 1.24s total · 0.80s daemon wait · 0.40s eval`.
pub(crate) fn print_timing(
    style: Style,
    total: Duration,
    done: &ReplDone,
) -> buck2_error::Result<()> {
    let text = format!(
        "time: {:.3}s total · {:.3}s daemon wait · {:.3}s eval",
        total.as_secs_f64(),
        Duration::from_millis(done.wait_ms).as_secs_f64(),
        Duration::from_millis(done.eval_ms).as_secs_f64(),
    );
    print_note(style, &text)
}

/// Prints `text` on stdout, on its own lines.
pub(crate) fn print_text(text: &str) -> buck2_error::Result<()> {
    buck2_client_ctx::println!("{}", text.trim_end_matches('\n'))
}

fn print_value(value: &ReplValue, style: Style) -> buck2_error::Result<()> {
    let text = value.text.trim_end_matches('\n');
    let (shown, omitted) = if style.interactive {
        truncate_lines(text, MAX_VALUE_LINES)
    } else {
        (text, 0)
    };
    buck2_client_ctx::println!("{}", shown.trim_end_matches('\n'))?;
    if omitted > 0 {
        buck2_client_ctx::println!("… {} more lines (:p _ to show all)", omitted)?;
    } else if value.truncated {
        buck2_client_ctx::println!("... (value truncated; `:print _` shows all of it)")?;
    }
    Ok(())
}

/// `(1.24s)` for a slow input, or `(1.24s, 0.80s waiting for the daemon)` if it waited long.
fn print_duration(done: &ReplDone, style: Style) -> buck2_error::Result<()> {
    let wait = Duration::from_millis(done.wait_ms);
    let total = wait.saturating_add(Duration::from_millis(done.eval_ms));
    if total < SHOW_DURATION {
        return Ok(());
    }
    let text = if wait >= SHOW_WAIT {
        format!(
            "({:.2}s, {:.2}s waiting for the daemon)",
            total.as_secs_f64(),
            wait.as_secs_f64()
        )
    } else {
        format!("({:.2}s)", total.as_secs_f64())
    };
    print_note(style, &text)
}

/// Prints `message` on stderr, on its own line (in red on a terminal).
pub(crate) fn print_error(style: Style, message: &str) -> buck2_error::Result<()> {
    buck2_client_ctx::eprintln!("{}", style.paint(RED, message.trim_end_matches('\n')))
}

/// Prints a note (dim on a terminal) on stderr.
pub(crate) fn print_note(style: Style, text: &str) -> buck2_error::Result<()> {
    buck2_client_ctx::eprintln!("{}", style.paint(DIM, text))
}

pub(crate) fn print_notice(style: Style, notice: &ReplNotice) -> buck2_error::Result<()> {
    let prefix = match notice.level() {
        repl_notice::Level::Info => "note",
        repl_notice::Level::Warning => "warning",
    };
    print_note(style, &format!("{prefix}: {}", notice.text))
}

pub(crate) fn flush() -> buck2_error::Result<()> {
    buck2_client_ctx::stdio::flush()
}
