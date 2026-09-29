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
//! This is the protocol stub: it answers `Open` with `Ready`, echoes every `Eval` input back as a
//! string value and answers every `Complete` with no candidates. The session proper replaces it.

use buck2_cli_proto::ReplDone;
use buck2_cli_proto::ReplMessage;
use buck2_cli_proto::ReplReady;
use buck2_cli_proto::ReplRequest;
use buck2_cli_proto::ReplResponse;
use buck2_cli_proto::ReplValue;
use buck2_cli_proto::repl_completions;
use buck2_cli_proto::repl_done;
use buck2_cli_proto::repl_message;
use buck2_cli_proto::repl_request;
use buck2_events::dispatch::span_async;
use buck2_server_ctx::commands::command_end;
use buck2_server_ctx::ctx::ServerCommandContextTrait;
use buck2_server_ctx::partial_result_dispatcher::PartialResultDispatcher;
use buck2_server_ctx::streaming_request_handler::StreamingRequestHandler;
use futures::StreamExt;

pub(crate) async fn repl_command(
    sctx: &dyn ServerCommandContextTrait,
    prd: PartialResultDispatcher<ReplMessage>,
    req: StreamingRequestHandler<ReplRequest>,
) -> buck2_error::Result<ReplResponse> {
    let start = sctx
        .command_start_event(buck2_data::ReplCommandStart {}.into())
        .await?;
    span_async(start, async move {
        let result = stub_loop(prd, req).await;
        let end = command_end(&result, buck2_data::ReplCommandEnd {});
        (result, end)
    })
    .await
}

async fn stub_loop(
    mut prd: PartialResultDispatcher<ReplMessage>,
    mut req: StreamingRequestHandler<ReplRequest>,
) -> buck2_error::Result<ReplResponse> {
    // EOF, a stream error and `Hangup` all end the session.
    while let Some(Ok(request)) = req.next().await {
        let id = request.id;
        let message = match request.request {
            Some(repl_request::Request::Open(_)) => repl_message::Message::Ready(ReplReady {
                cwd: String::new(),
                target_platform: String::new(),
                prelude_loaded: false,
            }),
            Some(repl_request::Request::Eval(eval)) => repl_message::Message::Done(ReplDone {
                outcome: Some(repl_done::Outcome::Value(ReplValue {
                    r#type: "str".to_owned(),
                    text: eval.input,
                    truncated: false,
                    json: None,
                })),
                ..ReplDone::default()
            }),
            Some(repl_request::Request::Complete(_)) => {
                repl_message::Message::Completions(buck2_cli_proto::ReplCompletions {
                    status: repl_completions::Status::Ok as i32,
                    candidates: Vec::new(),
                    message: String::new(),
                })
            }
            // Nothing is ever in flight, so there is nothing to interrupt.
            Some(repl_request::Request::Interrupt(_)) => continue,
            Some(repl_request::Request::Hangup(_)) => break,
            None => continue,
        };
        prd.emit(ReplMessage {
            id,
            message: Some(message),
        });
    }
    Ok(ReplResponse {})
}
