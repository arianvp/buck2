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

mod editor;
mod render;
mod script;
mod session;

/// Start an interactive BXL/Starlark session: evaluate code with `ctx` bound, run queries,
/// builds and binaries.
#[derive(Debug, clap::Parser)]
#[clap(name = "repl")]
pub struct ReplCommand {
    /// Evaluate INPUT and exit (repeatable).
    #[clap(short = 'e', long = "eval", value_name = "INPUT")]
    eval: Vec<String>,

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
        session::run(self, context, buckd, events_ctx).await
    }

    fn console_opts(&self) -> &CommonConsoleOptions {
        // The REPL owns the terminal: a superconsole would redraw over the prompt, and an
        // interactive console would read stdin.
        static SIMPLE: CommonConsoleOptions = CommonConsoleOptions {
            console_type: ConsoleType::Simple,
            ui: vec![],
            no_interactive_console: true,
        };
        static NONE: CommonConsoleOptions = CommonConsoleOptions {
            console_type: ConsoleType::None,
            ui: vec![],
            no_interactive_console: true,
        };
        if self.json { &NONE } else { &SIMPLE }
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
        vec![self.shutdown.subscriber()]
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
