/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Server side of `buck2 repl`.
//!
//! A session is one command. Its [`driver`] answers the client's requests; each input is
//! evaluated in a transaction of its own by the session [`thread`], which owns the Starlark
//! module that keeps the session's bindings (see [`session`]).

use buck2_cli_proto::ReplMessage;
use buck2_cli_proto::ReplRequest;
use buck2_cli_proto::ReplResponse;
use buck2_events::dispatch::span_async;
use buck2_server_ctx::commands::command_end;
use buck2_server_ctx::ctx::ServerCommandContextTrait;
use buck2_server_ctx::partial_result_dispatcher::PartialResultDispatcher;
use buck2_server_ctx::streaming_request_handler::StreamingRequestHandler;
use dupe::Dupe;

use crate::repl::driver::Driver;
use crate::repl::output::ReplEmitter;

mod cancel;
mod commands;
mod complete;
mod driver;
mod line_ctx;
mod output;
mod prep;
mod render;
mod session;
mod thread;

pub(crate) async fn repl_command(
    sctx: &dyn ServerCommandContextTrait,
    // The session's messages go to the same event dispatcher through `ReplEmitter`, which, unlike
    // this, the session thread can hold too.
    _prd: PartialResultDispatcher<ReplMessage>,
    req: StreamingRequestHandler<ReplRequest>,
) -> buck2_error::Result<ReplResponse> {
    let start = sctx
        .command_start_event(buck2_data::ReplCommandStart {}.into())
        .await?;
    let emitter = ReplEmitter::new(sctx.events().dupe());
    span_async(start, async move {
        // Cancellation of the command (e.g. the client going away) is observed by the driver,
        // which winds the session down; the session is never dropped midway (INV-8).
        let result = sctx
            .cancellation_context()
            .with_structured_cancellation(|observer| {
                Driver::new(sctx, req, observer, emitter).run()
            })
            .await;
        let end = command_end(&result, buck2_data::ReplCommandEnd {});
        (result, end)
    })
    .await
}
