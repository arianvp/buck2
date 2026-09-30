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
//! Names, attributes and keyword arguments are completed by the session thread from the
//! session's module, live values and the documentation of the globals ([`names`]); no code runs.
//! Target patterns, modules to load and their symbols are completed by the driver in a
//! transaction of its own ([`targets`], [`loads`]). The functions of the query languages are
//! answered at once ([`query`]).

pub(crate) mod candidates;
pub(crate) mod loads;
pub(crate) mod names;
pub(crate) mod private;
pub(crate) mod query;
pub(crate) mod signature;
pub(crate) mod targets;
pub(crate) mod types;
