# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@prelude//erlang:erlang_erts.bzl", "erlang_erts")
load("@prelude//erlang:erlang_info.bzl", "ErlangOTPBinariesInfo")
load("@prelude//erlang:erlang_toolchain.bzl", "erlang_toolchain")
load(":archive.bzl", "MiseToolInfo", "mise_tool")

# The prebuilt OTP releases mise locks (builds.hex.pm, erlef/otp_builds) have
# to be installed with `Install <root>`, which bakes the absolute root into
# `bin/erl`. Install for a placeholder root instead, and replace `erl` with
# `dyn_erl`, which finds the root relative to its own location. `erlc` and
# `escript` run the `erl` next to them, so all three can run from anywhere.
_RELOCATABLE_INSTALL = [
    "if [ -x Install ]; then",
    "  ./Install -cross -minimal /mise-erlang-root >/dev/null",
    "  for erts in erts-*; do",
    "    cp \"$erts/bin/dyn_erl\" \"$erts/bin/erl\"",
    "    cp \"$erts/bin/dyn_erl\" bin/erl",
    "  done",
    "fi",
]

def _mise_erlang_binaries_impl(ctx: AnalysisContext) -> list[Provider]:
    otp = ctx.attrs.otp[MiseToolInfo]
    return [
        DefaultInfo(),
        ErlangOTPBinariesInfo(
            erl = cmd_args(otp.bins["erl"]),
            erlc = cmd_args(otp.bins["erlc"]),
            escript = cmd_args(otp.bins["escript"]),
        ),
    ]

mise_erlang_binaries = rule(
    impl = _mise_erlang_binaries_impl,
    doc = "The OTP binaries of an Erlang/OTP installed by `mise_tool`, for `erlang_toolchain`.",
    attrs = {
        "otp": attrs.exec_dep(providers = [MiseToolInfo]),
    },
    is_toolchain_rule = True,
)

def mise_erlang_toolchain(
        name: str,
        lock: dict,
        tool: str = "erlang",
        version: str | None = None,
        visibility: list[str] = ["PUBLIC"],
        **kwargs):
    """
    An Erlang toolchain using the Erlang/OTP pinned in a mise lock file.

    mise's `core:erlang` backend locks prebuilt releases for Linux (glibc)
    and macOS. Those need a POSIX shell to install, so Windows execution
    platforms are not supported.

    The Erlang rules use the toolchain named `erlang-default`:

    ```python
    load("@prelude//toolchains/mise:defs.bzl", "mise_erlang_toolchain")
    load(":mise.lock.toml", mise_lock = "value")

    mise_erlang_toolchain(name = "erlang-default", lock = mise_lock)
    ```

    Other arguments are forwarded to `erlang_toolchain`.
    """
    mise_tool(
        name = name + "-otp",
        lock = lock,
        tool = tool,
        version = version,
        post_extract = _RELOCATABLE_INSTALL,
        bins = {
            "erl": "bin/erl{exe}",
            "erlc": "bin/erlc{exe}",
            "escript": "bin/escript{exe}",
        },
        visibility = visibility,
    )
    mise_erlang_binaries(
        name = name + "-binaries",
        otp = ":{}-otp".format(name),
        visibility = visibility,
    )
    erlang_erts(
        name = name + "-erts",
        otp_binaries = ":{}-binaries".format(name),
        visibility = visibility,
    )
    erlang_toolchain(
        name = name,
        erts_toolchain_info = ":{}-erts".format(name),
        otp_binaries = ":{}-binaries".format(name),
        parse_transforms = kwargs.pop("parse_transforms", []),
        parse_transforms_filters = kwargs.pop("parse_transforms_filters", {}),
        visibility = visibility,
        **kwargs,
    )
