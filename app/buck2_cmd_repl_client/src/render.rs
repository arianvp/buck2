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

use buck2_cli_proto::ReplDone;
use buck2_cli_proto::ReplNotice;
use buck2_cli_proto::ReplValue;
use buck2_cli_proto::repl_done;
use buck2_cli_proto::repl_error;
use buck2_cli_proto::repl_notice;

/// How an input went.
pub(crate) enum Rendered {
    Ok,
    Failed,
    Interrupted,
}

/// Prints the result of an input.
///
/// Fails if the output cannot be written (e.g. stdout is a closed pipe): the caller stops, since
/// nobody sees the results of later inputs.
pub(crate) fn render_done(done: &ReplDone) -> buck2_error::Result<Rendered> {
    Ok(match &done.outcome {
        None => Rendered::Ok,
        Some(repl_done::Outcome::Value(value)) => {
            print_value(value)?;
            Rendered::Ok
        }
        Some(repl_done::Outcome::Error(error)) => {
            if error.kind() == repl_error::Kind::Interrupted {
                print_error("interrupted")?;
                Rendered::Interrupted
            } else {
                print_error(&error.message)?;
                Rendered::Failed
            }
        }
        Some(repl_done::Outcome::Run(run)) => {
            // Running the program is not implemented yet: print its command line.
            let argv = run.argv.iter().map(String::as_str);
            let command = shlex::try_join(argv).unwrap_or_else(|_| run.argv.join(" "));
            buck2_client_ctx::println!("{}", command)?;
            Rendered::Ok
        }
    })
}

fn print_value(value: &ReplValue) -> buck2_error::Result<()> {
    buck2_client_ctx::println!("{}", value.text.trim_end_matches('\n'))?;
    if value.truncated {
        buck2_client_ctx::println!("... (value truncated; `:print _` shows all of it)")?;
    }
    Ok(())
}

/// Prints `message` on stderr, on its own line.
pub(crate) fn print_error(message: &str) -> buck2_error::Result<()> {
    buck2_client_ctx::eprintln!("{}", message.trim_end_matches('\n'))
}

pub(crate) fn print_notice(notice: &ReplNotice) -> buck2_error::Result<()> {
    let prefix = match notice.level() {
        repl_notice::Level::Info => "note",
        repl_notice::Level::Warning => "warning",
    };
    buck2_client_ctx::eprintln!("{}: {}", prefix, notice.text)
}

pub(crate) fn flush() -> buck2_error::Result<()> {
    buck2_client_ctx::stdio::flush()
}
