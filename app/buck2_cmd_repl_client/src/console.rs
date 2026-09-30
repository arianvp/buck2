/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! What the session writes to the terminal on the client's runtime: the output of inputs, the
//! messages of the SIGINT handler and, when the session shows live progress, the events of the
//! daemon.
//!
//! With live progress (`--console auto` in the line editor on a terminal, or `--console super`),
//! a superconsole shows what the daemon does while an input runs (build progress, running
//! actions), as `buck2 build` does. It is created when the input starts, drawn once the input
//! has run for [`SHOW_AFTER`] (quick inputs draw nothing), and erased before the result of the
//! input is passed on to be printed. Between inputs nothing is drawn: a simple console prints
//! the events, as the session's console does without live progress, and it is not ticked (it
//! would print `Waiting on ...` over the prompt).
//!
//! The terminal has one writer at a time: everything here runs on the client's runtime (the event
//! subscriber, the partial result handler and the SIGINT handler, which never run at the same
//! time), and while a superconsole is shown the input thread writes nothing. It only prints a
//! result after the `ReplDone` of its input, which is passed on once the superconsole is erased,
//! and a notice (the only thing it prints while an input runs) ends the superconsole of its input
//! before it is passed on. The output of an input (`print()`, `ctx.output.print`, `:p`) is
//! written with the canvas taken off the terminal; the next tick draws it again below, unless the
//! output left a line unfinished.

use std::io::IsTerminal;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use async_trait::async_trait;
use buck2_cli_proto::ReplOutput;
use buck2_cli_proto::repl_output;
use buck2_client_ctx::common::ui::ConsoleType;
use buck2_client_ctx::common::ui::get_console_with_root;
use buck2_client_ctx::exit_result::ClientIoError;
use buck2_client_ctx::subscribers::subscriber::EventSubscriber;
use buck2_client_ctx::subscribers::superconsole::StatefulSuperConsole;
use buck2_client_ctx::subscribers::superconsole::SuperConsoleConfig;
use buck2_client_ctx::subscribers::superconsole::timekeeper::RealtimeClock;
use buck2_client_ctx::subscribers::superconsole::timekeeper::Timekeeper;
use buck2_client_ctx::ticker::Tick;
use buck2_event_observer::span_tracker::EventTimestamp;
use buck2_event_observer::verbosity::Verbosity;
use buck2_events::BuckEvent;
use buck2_wrapper_common::invocation_id::TraceId;
use dupe::Dupe;

use crate::session::SharedUi;

/// An input shows its superconsole once it has run this long, so that quick inputs do not
/// flash one.
const SHOW_AFTER: Duration = Duration::from_millis(300);

/// The name the consoles show (`Command repl`).
const COMMAND_NAME: &str = "repl";

/// Where the session writes on the client's runtime. Without [`start`](Self::start) (no live
/// progress), output and messages are written as they come.
#[derive(Clone, Dupe, Default)]
pub(crate) struct ReplConsole(Arc<tokio::sync::Mutex<State>>);

impl std::fmt::Debug for ReplConsole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReplConsole")
    }
}

/// How the session shows live progress.
pub(crate) struct Setup {
    pub(crate) trace_id: TraceId,
    pub(crate) verbosity: Verbosity,
    /// `--ui` and friends.
    pub(crate) config: SuperConsoleConfig,
    /// Show a superconsole even if stderr is not a terminal (`--console super`).
    pub(crate) forced: bool,
    /// Which input runs.
    pub(crate) ui: SharedUi,
}

#[derive(Default)]
struct State {
    setup: Option<Setup>,
    display: Display,
    /// No superconsole is created for the requests up to this id (their results have arrived, or
    /// none could be created for them).
    done_up_to: u64,
    /// A superconsole could not be created: the session goes on without one.
    disabled: bool,
}

#[derive(Default)]
enum Display {
    /// No live progress (or not yet).
    #[default]
    Unset,
    /// No input runs, or the input that runs has no superconsole: a simple console prints the
    /// events.
    Simple(Box<dyn EventSubscriber>),
    /// Request `id` runs, and `console` shows it. After a notice, `console` is a simple console
    /// (it was finalized) until the input ends.
    Live {
        id: u64,
        console: StatefulSuperConsole,
        /// When the canvas is drawn first.
        show_at: Instant,
        /// Output written to the terminal did not end its line (on this channel): the canvas is
        /// not drawn below it until it does.
        open_line: Option<repl_output::Channel>,
    },
}

impl ReplConsole {
    /// Shows live progress from now on.
    pub(crate) async fn start(&self, setup: Setup) {
        let mut state = self.0.lock().await;
        state.display = Display::Simple(simple_console(&setup));
        state.setup = Some(setup);
    }

    /// The subscriber that passes the events on to the consoles.
    pub(crate) fn subscriber(&self) -> Box<dyn EventSubscriber> {
        Box::new(ConsoleSubscriber(self.dupe()))
    }

    /// Writes the output of an input.
    pub(crate) async fn write_output(&self, output: &ReplOutput) -> buck2_error::Result<()> {
        let mut state = self.0.lock().await;
        state.follow_ui().await?;
        let channel = output.channel();
        if let Display::Live {
            console, open_line, ..
        } = &mut state.display
            && is_running(console)
            && is_terminal(channel)
        {
            if output.data.is_empty() {
                return Ok(());
            }
            if open_line.is_none() {
                console.clear()?;
            }
            write_raw(channel, &output.data)?;
            *open_line = if output.data.ends_with(b"\n") {
                None
            } else {
                Some(channel)
            };
            return Ok(());
        }
        write_raw(channel, &output.data)
    }

    /// A notice of the input that runs is passed on to be printed: its superconsole ends.
    pub(crate) async fn before_notice(&self) -> buck2_error::Result<()> {
        let mut state = self.0.lock().await;
        if let Display::Live {
            console, open_line, ..
        } = &mut state.display
            && is_running(console)
        {
            end_line(open_line)?;
            console.erase_interactive_output().await?;
        }
        Ok(())
    }

    /// The result of request `id` is passed on to be printed: its superconsole is erased.
    pub(crate) async fn before_done(&self, id: u64) -> buck2_error::Result<()> {
        let mut state = self.0.lock().await;
        state.done_up_to = state.done_up_to.max(id);
        match &state.display {
            Display::Live { id: live, .. } if *live == id => state.end_input().await,
            _ => Ok(()),
        }
    }

    /// Prints a message of the SIGINT handler (Ctrl-C) on stderr, above the canvas.
    pub(crate) async fn message(&self, text: &str) -> buck2_error::Result<()> {
        let mut state = self.0.lock().await;
        if let Display::Live {
            console, open_line, ..
        } = &mut state.display
        {
            end_line(open_line)?;
            console.clear()?;
        }
        buck2_client_ctx::eprintln!("{}", text)
    }

    /// The daemon call is over: the superconsole of an input that ran is erased.
    pub(crate) async fn end(&self) -> buck2_error::Result<()> {
        self.0.lock().await.end_input().await
    }
}

impl State {
    /// Creates the superconsole of the input that runs, or ends one whose input is over.
    async fn follow_ui(&mut self) -> buck2_error::Result<()> {
        let Some(setup) = &self.setup else {
            return Ok(());
        };
        let busy = setup.ui.busy();
        match &self.display {
            Display::Live { id, .. } => {
                if busy != Some(*id) {
                    // The input is over without a result (e.g. the session ended).
                    self.end_input().await?;
                }
            }
            Display::Simple(_) | Display::Unset => {
                if let Some(id) = busy
                    && id > self.done_up_to
                {
                    self.start_input(id);
                }
            }
        }
        Ok(())
    }

    /// Request `id` runs: a superconsole shows it.
    fn start_input(&mut self, id: u64) {
        let Some(setup) = &self.setup else {
            return;
        };
        // Whatever happens, this input gets no other superconsole.
        self.done_up_to = id;
        if self.disabled {
            return;
        }
        let timekeeper = Timekeeper::new(
            Box::new(RealtimeClock),
            EventTimestamp(SystemTime::now().into()),
        );
        match StatefulSuperConsole::new_blocking(
            setup.trace_id.dupe(),
            COMMAND_NAME,
            setup.verbosity,
            false,
            timekeeper,
            setup.config.clone(),
            setup.forced,
        ) {
            Ok(Some(console)) => {
                self.display = Display::Live {
                    id,
                    console,
                    show_at: Instant::now() + SHOW_AFTER,
                    open_line: None,
                };
            }
            // stderr is not a terminal that can show one (any more).
            Ok(None) => self.disabled = true,
            Err(e) => {
                self.disabled = true;
                let _ignored = buck2_client_ctx::eprintln!(
                    "warning: cannot show the progress of inputs: {:#}",
                    e
                );
            }
        }
    }

    /// Erases the superconsole of the input that ran, if any, and goes on with a new simple
    /// console, which knows nothing of the input (so that it never waits for its work).
    async fn end_input(&mut self) -> buck2_error::Result<()> {
        if !matches!(self.display, Display::Live { .. }) {
            return Ok(());
        }
        let next = match &self.setup {
            Some(setup) => Display::Simple(simple_console(setup)),
            None => Display::Unset,
        };
        let Display::Live {
            mut console,
            mut open_line,
            ..
        } = std::mem::replace(&mut self.display, next)
        else {
            return Ok(());
        };
        end_line(&mut open_line)?;
        console.erase_interactive_output().await
    }

    /// The console that the events go to now, and whether it is to be ticked.
    fn console(&mut self) -> Option<(&mut dyn EventSubscriber, bool)> {
        let busy = self.setup.as_ref().is_some_and(|s| s.ui.busy().is_some());
        match &mut self.display {
            Display::Unset => None,
            // Ticked only while an input runs: at the prompt, it would print `Waiting on ...`.
            Display::Simple(console) => Some((console.as_mut(), busy)),
            Display::Live {
                console,
                show_at,
                open_line,
                ..
            } => {
                let tick = open_line.is_none() && Instant::now() >= *show_at;
                Some((console, tick))
            }
        }
    }
}

/// The simple console that prints the events between inputs (or during an input without a
/// superconsole), as the session's console does without live progress.
fn simple_console(setup: &Setup) -> Box<dyn EventSubscriber> {
    let timekeeper = Timekeeper::new(
        Box::new(RealtimeClock),
        EventTimestamp(SystemTime::now().into()),
    );
    get_console_with_root(
        setup.trace_id.dupe(),
        ConsoleType::Simple,
        setup.verbosity,
        false,
        timekeeper,
        COMMAND_NAME,
        setup.config.clone(),
        None,
    )
    .0
}

fn is_running(console: &StatefulSuperConsole) -> bool {
    matches!(console, StatefulSuperConsole::Running(_))
}

/// Whether what is written to `channel` shows on a terminal, where it could be mixed with the
/// canvas (which is drawn on stderr).
fn is_terminal(channel: repl_output::Channel) -> bool {
    match channel {
        repl_output::Channel::Stdout => std::io::stdout().is_terminal(),
        repl_output::Channel::Stderr => std::io::stderr().is_terminal(),
    }
}

/// Ends the line that output left unfinished, so that the canvas is not drawn over it (a canvas
/// starts at the beginning of the line of the cursor, and clears everything below it).
fn end_line(open_line: &mut Option<repl_output::Channel>) -> buck2_error::Result<()> {
    match open_line.take() {
        Some(channel) => write_raw(channel, b"\n"),
        None => Ok(()),
    }
}

/// Writes (and flushes) output of the daemon as it is.
pub(crate) fn write_raw(channel: repl_output::Channel, data: &[u8]) -> buck2_error::Result<()> {
    match channel {
        repl_output::Channel::Stdout => {
            buck2_client_ctx::stdio::print_bytes(data)?;
            buck2_client_ctx::stdio::flush()
        }
        repl_output::Channel::Stderr => {
            let mut stderr = std::io::stderr().lock();
            stderr
                .write_all(data)
                .and_then(|()| stderr.flush())
                .map_err(|e| ClientIoError::from(e).into())
        }
    }
}

/// Passes the events of the daemon on to the console that shows them now.
struct ConsoleSubscriber(ReplConsole);

#[async_trait]
impl EventSubscriber for ConsoleSubscriber {
    fn name(&self) -> &'static str {
        "repl-console"
    }

    async fn handle_events(&mut self, events: &[Arc<BuckEvent>]) -> buck2_error::Result<()> {
        let mut state = self.0.0.lock().await;
        state.follow_ui().await?;
        match state.console() {
            Some((console, _)) => console.handle_events(events).await,
            None => Ok(()),
        }
    }

    async fn handle_tailer_stderr(&mut self, stderr: &str) -> buck2_error::Result<()> {
        let mut state = self.0.0.lock().await;
        match state.console() {
            Some((console, _)) => console.handle_tailer_stderr(stderr).await,
            // E.g. `Starting new buck2 daemon...`, before the session starts.
            None => buck2_client_ctx::eprintln!("{}", stderr),
        }
    }

    async fn tick(&mut self, tick: &Tick) -> buck2_error::Result<()> {
        let mut state = self.0.0.lock().await;
        state.follow_ui().await?;
        match state.console() {
            Some((console, true)) => console.tick(tick).await,
            _ => Ok(()),
        }
    }

    async fn handle_command_result(
        &mut self,
        _result: &buck2_cli_proto::CommandResult,
    ) -> buck2_error::Result<()> {
        // Not passed on: the simple console would print a summary of the failed actions of the
        // last input (each input showed its own errors). The session's console (`none`) prints
        // the error of the command, if any.
        self.0.end().await
    }

    async fn handle_error(&mut self, _error: &buck2_error::Error) -> buck2_error::Result<()> {
        self.0.end().await
    }

    async fn erase_interactive_output(&mut self) -> buck2_error::Result<()> {
        self.0.end().await
    }

    async fn finalize(self: Box<Self>) -> buck2_error::Result<()> {
        self.0.end().await
    }
}
