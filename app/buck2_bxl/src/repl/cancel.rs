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
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use dice_futures::cancellation::CancellationHandle;

/// Cancels the request in flight (an interrupt, or the end of the session).
///
/// The request's DICE work is never dropped from outside (INV-8): [`trigger`](Self::trigger)
/// cancels the task that runs it through its [`CancellationHandle`], which notifies the
/// task's structured cancellation observers; the work then winds down and replies.
pub(crate) struct EvalCancel {
    triggered: AtomicBool,
    handle: Mutex<Option<CancellationHandle>>,
}

impl EvalCancel {
    pub(crate) fn new() -> Self {
        EvalCancel {
            triggered: AtomicBool::new(false),
            handle: Mutex::new(None),
        }
    }

    pub(crate) fn trigger(&self) {
        if self.triggered.swap(true, Ordering::SeqCst) {
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
        self.triggered.load(Ordering::SeqCst)
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
