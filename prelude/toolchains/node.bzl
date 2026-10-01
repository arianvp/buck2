# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@prelude//js/npm:toolchain.bzl", "NodeToolchainInfo")

def _node_toolchain_impl(ctx: AnalysisContext) -> list[Provider]:
    distribution = ctx.attrs.distribution[DefaultInfo].default_outputs[0]
    return [
        DefaultInfo(),
        NodeToolchainInfo(
            node = distribution.project("bin/node"),
            # Just npm's own package (~2k files), not the whole distribution
            # (~4.7k files, mostly C headers): with remote execution, every
            # input is uploaded and materialized on the worker.
            npm = distribution.project("lib/node_modules/npm"),
            os = ctx.attrs.node_os,
            cpu = ctx.attrs.node_cpu,
        ),
    ]

# A Node.js toolchain from an unpacked official distribution (for example an
# `http_archive` of https://nodejs.org/dist/vX.Y.Z/node-vX.Y.Z-linux-x64.tar.gz
# with `strip_prefix`). Only the Unix layout (`bin/node`) is supported.
node_toolchain = rule(
    impl = _node_toolchain_impl,
    attrs = {
        "distribution": attrs.dep(providers = [DefaultInfo]),
        "node_cpu": attrs.string(doc = "`process.arch` of the target, e.g. `x64` or `arm64`."),
        "node_os": attrs.string(doc = "`process.platform` of the target, e.g. `linux` or `darwin`."),
    },
    is_toolchain_rule = True,
)
