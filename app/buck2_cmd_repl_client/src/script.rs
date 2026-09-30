/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Non-interactive mode: evaluates the files and the `-e` inputs of the command line, then (unless
//! only `-e` inputs were given) the inputs piped on stdin, split with the [`Chunker`] as lines
//! arrive, so that a program can drive the session, one at a time.

use std::io::BufRead;

use buck2_repl_syntax::chunker::Chunker;

use crate::inputs::FirstInputs;
use crate::inputs::Inputs;
use crate::inputs::Next;
use crate::render;
use crate::session::InputOutcome;

pub(crate) struct ScriptMode {
    pub(crate) inputs: Inputs,
    /// The files and `-e` inputs of the command line, evaluated first.
    pub(crate) first: FirstInputs,
    /// Then read inputs from stdin.
    pub(crate) read_stdin: bool,
    pub(crate) outcome_tx: tokio::sync::oneshot::Sender<InputOutcome>,
}

impl ScriptMode {
    /// Runs on the input thread. Reports the outcome, then hangs up.
    pub(crate) fn run(self) {
        let ScriptMode {
            mut inputs,
            first,
            read_stdin,
            outcome_tx,
        } = self;
        run(&mut inputs, first, read_stdin);
        inputs.finish_session(outcome_tx);
    }
}

fn run(inputs: &mut Inputs, first: FirstInputs, read_stdin: bool) {
    let Some((_ready, notices)) = inputs.wait_for_ready() else {
        return;
    };
    inputs.print_notices(&notices);
    if inputs.outcome.output_error.is_some() {
        return;
    }
    if let Next::Stop = inputs.run_first(first) {
        return;
    }
    if !read_stdin {
        return;
    }
    let mut chunker = Chunker::new();
    let mut lines = std::io::stdin().lock().lines();
    loop {
        // The session does not wait for a script that waits for stdin: once it has ended, the
        // script must not start another input (its result could be cut short, e.g. half of a
        // `--json` record printed).
        if !inputs.ui.start_reading_stdin() {
            inputs.outcome.lost = true;
            return;
        }
        let line = lines.next();
        if !inputs.ui.stop_reading_stdin() {
            inputs.outcome.lost = true;
            return;
        }
        let Some(line) = line else {
            break;
        };
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                let printed =
                    render::print_error(inputs.style(), &format!("error: cannot read stdin: {e}"));
                inputs.output(printed);
                inputs.outcome.failed = true;
                return;
            }
        };
        for chunk in chunker.push_line(&line) {
            if let Next::Stop = inputs.eval(chunk) {
                return;
            }
        }
    }
    if let Some(chunk) = chunker.finish() {
        let _ignored = inputs.eval(chunk);
    }
}
