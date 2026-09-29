# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

def _write_file(ctx):
    out = ctx.actions.write(ctx.attrs.out, ctx.attrs.content)
    return [DefaultInfo(default_output = out)]

write_file = rule(
    impl = _write_file,
    attrs = {
        "content": attrs.string(),
        "out": attrs.string(),
    },
)

def _runnable(_ctx):
    return [
        DefaultInfo(),
        RunInfo(args = cmd_args("echo", "hello from greet")),
    ]

runnable = rule(
    impl = _runnable,
    attrs = {},
)
