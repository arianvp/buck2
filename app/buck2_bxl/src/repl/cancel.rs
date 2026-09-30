/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Cancellation of one request.

use std::sync::Mutex;
use std::sync::PoisonError;

use dice_futures::cancellation::CancellationHandle;

/// Cancels the request in flight (an interrupt, or the end of the session).
///
/// The request's DICE work is never dropped from outside (INV-8): [`trigger`](Self::trigger)
/// cancels the task that runs it through its [`CancellationHandle`], which notifies the
/// task's structured cancellation observers; the work then winds down and replies. Work that is
/// safe to drop (DICE computations after the evaluation, such as materialization) races
/// [`cancelled`](Self::cancelled) instead.
pub(crate) struct EvalCancel {
    /// Set once, when the request is cancelled.
    triggered: tokio::sync::watch::Sender<bool>,
    handle: Mutex<Option<CancellationHandle>>,
}

impl EvalCancel {
    pub(crate) fn new() -> Self {
        EvalCancel {
            triggered: tokio::sync::watch::Sender::new(false),
            handle: Mutex::new(None),
        }
    }

    pub(crate) fn trigger(&self) {
        // Set before the handle is taken: `attach` checks it under the lock.
        if self.triggered.send_replace(true) {
            return;
        }
        let handle = self
            .handle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            handle.cancel();
        }
    }

    pub(crate) fn is_triggered(&self) -> bool {
        *self.triggered.borrow()
    }

    /// Resolves when the request is cancelled (at once if it already is).
    pub(crate) async fn cancelled(&self) {
        let mut triggered = self.triggered.subscribe();
        // Fails only if the sender is dropped, which `&self` prevents.
        let _ignored = triggered.wait_for(|triggered| *triggered).await;
    }

    /// Attaches the handle of the task that runs the request; cancels it at once if the request
    /// was already cancelled.
    pub(crate) fn attach(&self, handle: CancellationHandle) {
        let mut slot = self.handle.lock().unwrap_or_else(PoisonError::into_inner);
        if self.is_triggered() {
            drop(slot);
            handle.cancel();
        } else {
            *slot = Some(handle);
        }
    }
}
