/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Syntax helpers for `buck2 repl`, shared by the client and the daemon.
//!
//! Everything here is pure and total: no I/O, no Starlark dependency and no panics on any
//! input, because the daemon runs these functions on user input with `panic = "abort"`.
//!
//! - [`lexer`]: a tolerant Starlark tokenizer.
//! - [`highlight`]: what to colour in the line editor.
//! - [`completeness`]: whether an input buffer is ready to be submitted (the line editor's
//!   validator).
//! - [`chunker`]: splits piped input into inputs, line by line.
//! - [`text`]: dedent, input prechecks, string literals and output capping.
//! - [`commands`]: the meta-command table and its parser.
//! - [`site`]: what the cursor is on, for completion.
//! - [`query`]: where the cursor is in a query, for completion.
//! - [`matching`]: how well a completion candidate matches the word typed.
//! - [`candidates`]: text helpers for completion candidates.
//! - [`signature`]: signatures of functions, for the hint shown in a call.
//! - [`markdown`]: documentation (Markdown) rendered for the terminal.
//! - [`terminal`]: the columns and rows that text takes on a terminal.

pub mod candidates;
pub mod chunker;
pub mod commands;
pub mod completeness;
pub mod highlight;
pub mod lexer;
pub mod markdown;
pub mod matching;
mod nesting;
pub mod query;
pub mod signature;
pub mod site;
pub mod terminal;
pub mod text;
