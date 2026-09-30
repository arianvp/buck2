# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@prelude//rust:rust_toolchain.bzl", "PanicRuntime", "RustToolchainInfo")

_DEFAULT_TRIPLE = select({
    "prelude//os:linux": select({
        "prelude//cpu:arm64": "aarch64-unknown-linux-gnu",
        "prelude//cpu:riscv64": "riscv64gc-unknown-linux-gnu",
        "prelude//cpu:x86_64": "x86_64-unknown-linux-gnu",
    }),
    "prelude//os:macos": select({
        "prelude//cpu:arm64": "aarch64-apple-darwin",
        "prelude//cpu:x86_64": "x86_64-apple-darwin",
    }),
    "prelude//os:windows": select({
        "prelude//cpu:arm64": select({
            # Rustup's default ABI for the host on Windows is MSVC, not GNU.
            # When you do `rustup install stable` that's the one you get. It
            # makes you opt in to GNU by `rustup install stable-gnu`.
            "DEFAULT": "aarch64-pc-windows-msvc",
            "prelude//abi:gnu": "aarch64-pc-windows-gnu",
            "prelude//abi:msvc": "aarch64-pc-windows-msvc",
        }),
        "prelude//cpu:x86_64": select({
            "DEFAULT": "x86_64-pc-windows-msvc",
            "prelude//abi:gnu": "x86_64-pc-windows-gnu",
            "prelude//abi:msvc": "x86_64-pc-windows-msvc",
        }),
    }),
})

# Attributes of the toolchain rules that use `rust_toolchain_info`.
RUST_TOOLCHAIN_ATTRS = {
    "allow_lints": attrs.list(attrs.string(), default = []),
    "clippy_toml": attrs.option(attrs.dep(providers = [DefaultInfo]), default = None),
    "default_edition": attrs.option(attrs.string(), default = None),
    "deny_lints": attrs.list(attrs.string(), default = []),
    "doctests": attrs.bool(default = False),
    "nightly_features": attrs.bool(default = True),
    "report_unused_deps": attrs.bool(default = False),
    "rustc_binary_flags": attrs.list(attrs.arg(), default = []),
    "rustc_flags": attrs.list(attrs.arg(), default = []),
    "rustc_target_triple": attrs.string(default = _DEFAULT_TRIPLE),
    "rustc_test_flags": attrs.list(attrs.arg(), default = []),
    "rustdoc_flags": attrs.list(attrs.arg(), default = []),
    "warn_lints": attrs.list(attrs.string(), default = []),
}

def rust_toolchain_info(ctx, compiler: RunInfo, rustdoc: RunInfo, clippy_driver: RunInfo) -> RustToolchainInfo:
    """
    The `RustToolchainInfo` for a rule with `RUST_TOOLCHAIN_ATTRS`, running
    the given binaries.
    """
    return RustToolchainInfo(
        allow_lints = ctx.attrs.allow_lints,
        clippy_driver = clippy_driver,
        clippy_toml = ctx.attrs.clippy_toml[DefaultInfo].default_outputs[0] if ctx.attrs.clippy_toml else None,
        compiler = compiler,
        default_edition = ctx.attrs.default_edition,
        panic_runtime = PanicRuntime("unwind"),
        deny_lints = ctx.attrs.deny_lints,
        doctests = ctx.attrs.doctests,
        nightly_features = ctx.attrs.nightly_features,
        report_unused_deps = ctx.attrs.report_unused_deps,
        rustc_binary_flags = ctx.attrs.rustc_binary_flags,
        rustc_flags = ctx.attrs.rustc_flags,
        rustc_target_triple = ctx.attrs.rustc_target_triple,
        rustc_test_flags = ctx.attrs.rustc_test_flags,
        rustdoc = rustdoc,
        rustdoc_flags = ctx.attrs.rustdoc_flags,
        warn_lints = ctx.attrs.warn_lints,
    )

def _system_rust_toolchain_impl(ctx):
    return [
        DefaultInfo(),
        rust_toolchain_info(
            ctx,
            compiler = RunInfo(args = ["rustc"]),
            rustdoc = RunInfo(args = ["rustdoc"]),
            clippy_driver = RunInfo(args = ["clippy-driver"]),
        ),
    ]

system_rust_toolchain = rule(
    impl = _system_rust_toolchain_impl,
    attrs = RUST_TOOLCHAIN_ATTRS,
    is_toolchain_rule = True,
)
