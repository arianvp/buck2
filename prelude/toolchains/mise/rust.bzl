# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@prelude//toolchains:rust.bzl", "RUST_TOOLCHAIN_ATTRS", "rust_toolchain_info")
load(":archive.bzl", "MiseToolInfo", "mise_tool")

# Components of the standalone installer that the toolchain doesn't need; not
# unpacking them saves most of the time and space.
_UNUSED_COMPONENTS = [
    "cargo",
    "llvm-bitcode-linker-preview",
    "llvm-tools-preview",
    "rust-analysis-*",
    "rust-analyzer-preview",
    "rust-docs",
    "rust-docs-json-preview",
    "rustfmt-preview",
]

def _rust_install(components: list[str]) -> list[str]:
    # The standalone installers (`rust-<version>-<triple>.tar.xz`) hold one
    # directory per component and an `install.sh` that merges them into a
    # prefix. The result finds its sysroot relative to `bin/rustc`. Without an
    # `install.sh` the archive is assumed to be installed already.
    return [
        "if [ -f install.sh ]; then",
        "  components='{}'".format(",".join(components)),
        "  for c in $(cat components); do case \"$c\" in rust-std-*) components=\"$components,$c\" ;; esac; done",
        "  sh install.sh --prefix=\"$MISE_OUT\" --components=\"$components\" --disable-ldconfig >/dev/null",
        "fi",
    ]

def _mise_rust_toolchain_impl(ctx: AnalysisContext) -> list[Provider]:
    rust = ctx.attrs.distribution[MiseToolInfo]
    return [
        DefaultInfo(),
        rust_toolchain_info(
            ctx,
            compiler = rust.bins["rustc"],
            rustdoc = rust.bins["rustdoc"],
            clippy_driver = rust.bins["clippy-driver"],
        ),
    ]

_mise_rust_toolchain = rule(
    impl = _mise_rust_toolchain_impl,
    attrs = RUST_TOOLCHAIN_ATTRS | {
        "distribution": attrs.exec_dep(providers = [MiseToolInfo]),
    },
    is_toolchain_rule = True,
)

def mise_rust_toolchain(
        name: str,
        lock: dict,
        tool: str = "rust",
        version: str | None = None,
        components: list[str] = ["rustc", "clippy-preview"],
        visibility: list[str] = ["PUBLIC"],
        **kwargs):
    """
    A Rust toolchain using the Rust standalone installer pinned in a mise lock
    file.

    mise's `core:rust` backend installs through rustup and doesn't lock a
    download, so pin the standalone installers with the `http:` backend:

    ```toml
    # mise.toml
    [tools."http:rust"]
    version = "1.89.0"

    [tools."http:rust".platforms]
    linux-x64 = { url = "https://static.rust-lang.org/dist/rust-{{version}}-x86_64-unknown-linux-gnu.tar.xz", checksum = "sha256:c4f2796b..." }
    macos-arm64 = { url = "https://static.rust-lang.org/dist/rust-{{version}}-aarch64-apple-darwin.tar.xz", checksum = "sha256:a62f8ae2..." }
    ```

    The checksums are published next to each installer (`<url>.sha256`).

    ```python
    load("@prelude//toolchains/mise:defs.bzl", "mise_rust_toolchain")
    load(":mise.lock.toml", mise_lock = "value")

    mise_rust_toolchain(name = "rust", lock = mise_lock)
    ```

    `components` are installed along with the standard library for the
    installer's host. Other arguments, like `default_edition`, are the same as
    for `system_rust_toolchain`.
    """
    mise_tool(
        name = name + "_mise",
        lock = lock,
        tool = tool,
        version = version,
        excludes = ["*/{}/*".format(c) for c in _UNUSED_COMPONENTS if c not in components],
        post_extract = _rust_install(components),
        bins = {
            "clippy-driver": "bin/clippy-driver{exe}",
            "rustc": "bin/rustc{exe}",
            "rustdoc": "bin/rustdoc{exe}",
        },
        visibility = visibility,
    )
    _mise_rust_toolchain(
        name = name,
        distribution = ":{}_mise".format(name),
        visibility = visibility,
        **kwargs,
    )
