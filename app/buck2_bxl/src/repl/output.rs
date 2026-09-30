/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! How the session talks to the client: [`ReplEmitter`] sends `ReplMessage`s (from the driver
//! and from the session thread alike), and [`ReplPrintHandler`] turns `print()` into buffered
//! `ReplOutput`.

use std::cell::Cell;
use std::cell::RefCell;
use std::time::Duration;
use std::time::Instant;

use buck2_cli_proto::PartialResult;
use buck2_cli_proto::ReplDone;
use buck2_cli_proto::ReplMessage;
use buck2_cli_proto::ReplNotice;
use buck2_cli_proto::ReplOutput;
use buck2_cli_proto::repl_message;
use buck2_cli_proto::repl_notice;
use buck2_cli_proto::repl_output;
use buck2_events::dispatch::EventDispatcher;
use dupe::Dupe;
use starlark::PrintHandler;

/// Largest `ReplOutput` payload: large gRPC messages can end the stream.
pub(crate) const MAX_OUTPUT_CHUNK: usize = 16 << 10;

/// How long `print()` output may wait in the buffer while more of it arrives.
const PRINT_FLUSH_INTERVAL: Duration = Duration::from_millis(100);

/// Sends `ReplMessage`s to the client.
///
/// It sends straight into the command's event dispatcher (the one its
/// `PartialResultDispatcher` uses), so both the driver and the session thread can hold one.
#[derive(Clone, Dupe)]
pub(crate) struct ReplEmitter {
    dispatcher: EventDispatcher,
}

impl ReplEmitter {
    pub(crate) fn new(dispatcher: EventDispatcher) -> Self {
        ReplEmitter { dispatcher }
    }

    pub(crate) fn emit(&self, id: u64, message: repl_message::Message) {
        self.dispatcher.partial_result(PartialResult {
            partial_result: Some(
                ReplMessage {
                    id,
                    message: Some(message),
                }
                .into(),
            ),
        });
    }

    pub(crate) fn done(&self, id: u64, done: ReplDone) {
        self.emit(id, repl_message::Message::Done(done));
    }

    pub(crate) fn notice(&self, id: u64, level: repl_notice::Level, text: String) {
        self.emit(
            id,
            repl_message::Message::Notice(ReplNotice {
                level: level as i32,
                text,
            }),
        );
    }

    /// Sends `data` in chunks of at most [`MAX_OUTPUT_CHUNK`] bytes.
    pub(crate) fn output(&self, id: u64, channel: repl_output::Channel, data: &[u8]) {
        for chunk in data.chunks(MAX_OUTPUT_CHUNK) {
            self.emit(
                id,
                repl_message::Message::Output(ReplOutput {
                    channel: channel as i32,
                    data: chunk.to_vec(),
                }),
            );
        }
    }
}

/// `print()` for the evaluator of one input: output goes to the client's stdout, buffered and
/// sent when [`MAX_OUTPUT_CHUNK`] bytes have accumulated, when a line arrives
/// [`PRINT_FLUSH_INTERVAL`] after the last send, and at the end of the input
/// ([`flush`](Self::flush)).
pub(crate) struct ReplPrintHandler {
    emitter: ReplEmitter,
    id: u64,
    buf: RefCell<Vec<u8>>,
    last_flush: Cell<Instant>,
}

impl ReplPrintHandler {
    pub(crate) fn new(emitter: ReplEmitter, id: u64) -> Self {
        ReplPrintHandler {
            emitter,
            id,
            buf: RefCell::new(Vec::new()),
            last_flush: Cell::new(Instant::now()),
        }
    }

    pub(crate) fn flush(&self) {
        // Only this handler borrows the buffer, and never while calling out.
        if let Ok(mut buf) = self.buf.try_borrow_mut() {
            self.flush_buf(&mut buf);
        }
    }

    fn flush_buf(&self, buf: &mut Vec<u8>) {
        if !buf.is_empty() {
            self.emitter
                .output(self.id, repl_output::Channel::Stdout, buf);
            buf.clear();
        }
        self.last_flush.set(Instant::now());
    }
}

impl PrintHandler for ReplPrintHandler {
    fn println(&self, text: &str) -> starlark::Result<()> {
        if let Ok(mut buf) = self.buf.try_borrow_mut() {
            buf.extend_from_slice(text.as_bytes());
            buf.push(b'\n');
            if buf.len() >= MAX_OUTPUT_CHUNK
                || Instant::now() - self.last_flush.get() >= PRINT_FLUSH_INTERVAL
            {
                self.flush_buf(&mut buf);
            }
        }
        Ok(())
    }
}
