/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Client side of `buck2 repl`.

#![feature(used_with_arg)]

use std::io::IsTerminal;

use async_trait::async_trait;
use buck2_client_ctx::client_ctx::ClientCommandContext;
use buck2_client_ctx::common::BuckArgMatches;
use buck2_client_ctx::common::CommonBuildConfigurationOptions;
use buck2_client_ctx::common::CommonCommandOptions;
use buck2_client_ctx::common::CommonEventLogOptions;
use buck2_client_ctx::common::CommonStarlarkOptions;
use buck2_client_ctx::common::build::CommonBuildOptions;
use buck2_client_ctx::common::target_cfg::TargetCfgOptions;
use buck2_client_ctx::common::ui::CommonConsoleOptions;
use buck2_client_ctx::common::ui::ConsoleType;
use buck2_client_ctx::daemon::client::BuckdClientConnector;
use buck2_client_ctx::events_ctx::EventsCtx;
use buck2_client_ctx::exit_result::ExitResult;
use buck2_client_ctx::streaming::StreamingCommand;
use buck2_client_ctx::subscribers::subscriber::EventSubscriber;
use dupe::Dupe;

mod complete;
mod console;
mod editor;
mod help;
mod inputs;
mod render;
mod run;
mod script;
mod session;
mod settings;

/// Start an interactive BXL/Starlark session: evaluate code with `ctx` bound, run queries,
/// builds and binaries.
#[derive(Debug, clap::Parser)]
#[clap(name = "repl")]
pub struct ReplCommand {
    /// Files to evaluate before the first input: `.bzl` and `.bxl` files are loaded as `:load`
    /// loads them (their public symbols become bindings), other files are evaluated as one
    /// Starlark input (no `:` commands).
    #[clap(value_name = "FILES")]
    files: Vec<String>,

    /// Evaluate INPUT and exit (repeatable; after the FILES).
    #[clap(short = 'e', long = "eval", value_name = "INPUT")]
    eval: Vec<String>,

    /// Go on with the inputs of stdin (the prompt, on a terminal) after the `-e` inputs, as
    /// without `-e`: `buck2 repl -i -e INPUT FILE`.
    #[clap(short = 'i', long, overrides_with = "interactive")]
    interactive: bool,

    /// Print one JSON object per input (non-interactive only).
    #[clap(long)]
    json: bool,

    /// Non-interactive: keep going after a failing input (exit code is still non-zero).
    #[clap(long)]
    continue_on_error: bool,

    /// Do not read or write the history file.
    #[clap(long)]
    no_history: bool,

    /// Maximum Starlark heap for the session, in MiB.
    #[clap(long, default_value_t = 4096, value_name = "MIB")]
    max_heap_mb: u64,

    // Options for the builds of the session (`:build`, `:run`, `ctx.output.ensure`). They come
    // before `target_cfg`, whose help heading would otherwise cover them.
    #[clap(flatten)]
    build_opts: CommonBuildOptions,

    #[clap(flatten)]
    target_cfg: TargetCfgOptions,

    #[clap(flatten)]
    common_opts: CommonCommandOptions,

    #[clap(skip)]
    shutdown: session::ShutdownHangup,

    #[clap(skip)]
    console: console::ReplConsole,
}

/// How the session shows what the daemon does.
#[derive(Clone, Copy, Debug)]
enum ConsoleMode {
    /// A superconsole while each input runs, nothing between inputs (see [`console`]); `forced`:
    /// even when stderr is not a terminal.
    Live { forced: bool },
    /// The console of this type for the whole session, as other commands show theirs (never
    /// `auto` or `super`).
    Plain(ConsoleType),
}

impl ReplCommand {
    /// Whether inputs are read from stdin (the editor, if it is a terminal) after the files and
    /// the `-e` inputs: unless only `-e` inputs were given.
    fn reads_stdin(&self) -> bool {
        self.eval.is_empty() || self.interactive
    }

    /// Whether the inputs are read by the line editor.
    fn is_interactive(&self) -> bool {
        self.reads_stdin() && std::io::stdin().is_terminal()
    }

    fn console_mode(&self) -> ConsoleMode {
        if self.json {
            return ConsoleMode::Plain(ConsoleType::None);
        }
        match self.common_opts.console_opts.console_type {
            // Live progress only for the line editor with stdout and stderr on a terminal, where
            // the canvas can be erased before each result; scripts keep the simple console's
            // line by line output.
            ConsoleType::Auto => {
                if self.is_interactive()
                    && std::io::stderr().is_terminal()
                    && std::io::stdout().is_terminal()
                {
                    ConsoleMode::Live { forced: false }
                } else {
                    ConsoleMode::Plain(ConsoleType::Simple)
                }
            }
            ConsoleType::Super => ConsoleMode::Live { forced: true },
            console_type @ (ConsoleType::None
            | ConsoleType::Simple
            | ConsoleType::SimpleNoTty
            | ConsoleType::SimpleTty) => ConsoleMode::Plain(console_type),
        }
    }
}

#[async_trait(?Send)]
impl StreamingCommand for ReplCommand {
    const COMMAND_NAME: &'static str = "repl";

    async fn exec_impl(
        self,
        buckd: &mut BuckdClientConnector,
        matches: BuckArgMatches<'_>,
        ctx: &mut ClientCommandContext<'_>,
        events_ctx: &mut EventsCtx,
    ) -> ExitResult {
        if self.json {
            return ExitResult::err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "`buck2 repl --json` is not implemented yet"
            ));
        }
        let context = ctx.client_context(matches, &self)?;
        let trace_id = ctx.trace_id.dupe();
        session::run(self, context, trace_id, ctx.verbosity, buckd, events_ctx).await
    }

    fn console_opts(&self) -> &CommonConsoleOptions {
        // The REPL owns the terminal: the session's own console must not redraw (the live
        // progress of inputs is the `console` subscriber's, which draws only while an input
        // runs), and an interactive console would read stdin.
        static NONE: CommonConsoleOptions = CommonConsoleOptions {
            console_type: ConsoleType::None,
            ui: vec![],
            no_interactive_console: true,
        };
        static SIMPLE: CommonConsoleOptions = CommonConsoleOptions {
            console_type: ConsoleType::Simple,
            ui: vec![],
            no_interactive_console: true,
        };
        static SIMPLE_TTY: CommonConsoleOptions = CommonConsoleOptions {
            console_type: ConsoleType::SimpleTty,
            ui: vec![],
            no_interactive_console: true,
        };
        static SIMPLE_NO_TTY: CommonConsoleOptions = CommonConsoleOptions {
            console_type: ConsoleType::SimpleNoTty,
            ui: vec![],
            no_interactive_console: true,
        };
        match self.console_mode() {
            ConsoleMode::Live { .. } | ConsoleMode::Plain(ConsoleType::None) => &NONE,
            ConsoleMode::Plain(ConsoleType::SimpleTty) => &SIMPLE_TTY,
            ConsoleMode::Plain(ConsoleType::SimpleNoTty) => &SIMPLE_NO_TTY,
            ConsoleMode::Plain(ConsoleType::Simple | ConsoleType::Auto | ConsoleType::Super) => {
                &SIMPLE
            }
        }
    }

    fn event_log_opts(&self) -> &CommonEventLogOptions {
        &self.common_opts.event_log_opts
    }

    fn build_config_opts(&self) -> &CommonBuildConfigurationOptions {
        &self.common_opts.config_opts
    }

    fn starlark_opts(&self) -> &CommonStarlarkOptions {
        &self.common_opts.starlark_opts
    }

    fn extra_subscribers(&self) -> Vec<Box<dyn EventSubscriber>> {
        let mut subscribers = vec![self.shutdown.subscriber()];
        if let ConsoleMode::Live { .. } = self.console_mode() {
            subscribers.push(self.console.subscriber());
        }
        subscribers
    }

    fn handles_sigint(&self) -> bool {
        // Ctrl-C interrupts the input in flight, not the session.
        true
    }

    fn should_expect_spans(&self) -> bool {
        // An idle session has no spans: do not print "Waiting for daemon...".
        false
    }
}
