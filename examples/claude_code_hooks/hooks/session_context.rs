/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! SessionStart hook: whatever a SessionStart hook prints to stdout is added to
//! Claude's context at the start of the session.

fn main() {
    println!("This project builds with Buck2. Build with `buck2 build //...`; never invoke rustc or cargo directly.");
    println!(
        "Claude Code hooks live in BUCK as ordinary targets. After changing one, run `buck2 run //:claude_hooks` to install it."
    );
}
