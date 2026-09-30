/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Completion, and what the session knows about names and types (for `:doc` too).
//!
//! Names and attributes are completed by the session thread from the session's module, live
//! values and the documentation of the globals ([`names`]); no code runs. Target patterns are
//! completed by the driver in a transaction of its own ([`targets`]).

pub(crate) mod candidates;
pub(crate) mod names;
pub(crate) mod private;
pub(crate) mod targets;
pub(crate) mod types;
