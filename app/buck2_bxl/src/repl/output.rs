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

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::time::Duration;

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
use tokio::runtime::Handle;
use tokio::task::AbortHandle;
use tokio::time::MissedTickBehavior;

/// Largest `ReplOutput` payload: large gRPC messages can end the stream.
pub(crate) const MAX_OUTPUT_CHUNK: usize = 16 << 10;

/// How often buffered `print()` output is sent while an input runs.
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
/// sent when [`MAX_OUTPUT_CHUNK`] bytes have accumulated, by a timer every
/// [`PRINT_FLUSH_INTERVAL`] while the input runs (so output printed before long work is not held
/// back), and at the end of the input ([`flush`](Self::flush)).
pub(crate) struct ReplPrintHandler {
    buffer: Arc<PrintBuffer>,
    timer: AbortHandle,
}

struct PrintBuffer {
    emitter: ReplEmitter,
    id: u64,
    state: Mutex<PrintState>,
}

#[derive(Default)]
struct PrintState {
    data: Vec<u8>,
    /// The input is over: the timer sends nothing more.
    closed: bool,
}

impl PrintBuffer {
    fn lock(&self) -> MutexGuard<'_, PrintState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sends what is buffered. Called under the lock, so that chunks go out in order.
    fn send(&self, state: &mut PrintState) {
        if !state.data.is_empty() {
            self.emitter
                .output(self.id, repl_output::Channel::Stdout, &state.data);
            state.data.clear();
        }
    }
}

impl ReplPrintHandler {
    pub(crate) fn new(rt: &Handle, emitter: ReplEmitter, id: u64) -> Self {
        let buffer = Arc::new(PrintBuffer {
            emitter,
            id,
            state: Mutex::new(PrintState::default()),
        });
        // Weak: the timer never keeps the buffer (and its dispatcher) alive.
        let weak = Arc::downgrade(&buffer);
        let timer = rt.spawn(async move {
            let mut interval = tokio::time::interval(PRINT_FLUSH_INTERVAL);
            interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let Some(buffer) = weak.upgrade() else {
                    break;
                };
                let mut state = buffer.lock();
                if state.closed {
                    break;
                }
                buffer.send(&mut state);
            }
        });
        ReplPrintHandler {
            buffer,
            timer: timer.abort_handle(),
        }
    }

    /// Sends what is left. Nothing is sent after this returns.
    pub(crate) fn flush(&self) {
        let mut state = self.buffer.lock();
        self.buffer.send(&mut state);
        state.closed = true;
        drop(state);
        self.timer.abort();
    }
}

impl Drop for ReplPrintHandler {
    fn drop(&mut self) {
        self.timer.abort();
    }
}

impl PrintHandler for ReplPrintHandler {
    fn println(&self, text: &str) -> starlark::Result<()> {
        let mut state = self.buffer.lock();
        state.data.extend_from_slice(text.as_bytes());
        state.data.push(b'\n');
        if state.closed || state.data.len() >= MAX_OUTPUT_CHUNK {
            self.buffer.send(&mut state);
        }
        Ok(())
    }
}
