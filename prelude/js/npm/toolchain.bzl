# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

NodeToolchainInfo = provider(
    # @unsorted-dict-items
    fields = {
        # The `node` executable.
        "node": provider_field(Artifact),
        # npm's package directory (`lib/node_modules/npm` in a Node.js
        # distribution). `bin/npm-cli.js` is run with `node`; it loads the
        # rest of the package, so actions take the whole directory as input.
        "npm": provider_field(Artifact),
        # Target platform in Node.js terms (`process.platform` / `process.arch`),
        # used to pick optional platform-specific packages from lockfiles.
        "os": provider_field(str),
        "cpu": provider_field(str),
    },
)
