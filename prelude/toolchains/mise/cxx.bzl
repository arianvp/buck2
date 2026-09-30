# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@prelude//toolchains/cxx/zig:defs.bzl", "cxx_zig_toolchain", "zig_distribution")
load(":archive.bzl", "mise_tool")
load(":lock.bzl", "mise_platform_map", "mise_platform_select")

# mise platform components -> the names `zig_distribution` uses.
_ZIG_OS = {
    "freebsd": "freebsd",
    "linux": "linux",
    "macos": "macos",
    "windows": "windows",
}

_ZIG_ARCH = {
    "arm": "armv7a",
    "arm64": "aarch64",
    "armv7": "armv7a",
    "riscv64": "riscv64",
    "x64": "x86_64",
    "x86": "i386",
}

def mise_zig_distribution(
        name: str,
        lock: dict,
        tool: str = "zig",
        version: str | None = None,
        visibility: list[str] = ["PUBLIC"]):
    """
    The zig distribution pinned in a mise lock file, for `cxx_zig_toolchain`.
    """
    locked = mise_tool(
        name = name + "_mise",
        lock = lock,
        tool = tool,
        version = version,
        visibility = visibility,
    )
    platforms = locked.artifacts.keys()
    zig_distribution(
        name = name,
        dist = ":{}_mise".format(name),
        prefix = ".",
        suffix = mise_platform_select({p: ".exe" if p.startswith("windows-") else "" for p in platforms}, default = ""),
        os = mise_platform_select(mise_platform_map(platforms, lambda os, _arch, _libc: _ZIG_OS[os]), default = ""),
        arch = mise_platform_select(mise_platform_map(platforms, lambda _os, arch, _libc: _ZIG_ARCH[arch]), default = ""),
        version = locked.version,
        visibility = visibility,
    )
    return locked

def mise_cxx_zig_toolchain(
        name: str,
        lock: dict,
        tool: str = "zig",
        version: str | None = None,
        visibility: list[str] = ["PUBLIC"],
        **kwargs):
    """
    A C/C++ toolchain based on `zig cc`, using the zig pinned in a mise lock
    file. zig bundles clang and the libc headers and sources for all of its
    targets, so this does not rely on a system compiler or sysroot.

    ```python
    load("@prelude//toolchains/mise:defs.bzl", "mise_cxx_zig_toolchain")
    load(":mise.lock.toml", mise_lock = "value")

    mise_cxx_zig_toolchain(name = "cxx", lock = mise_lock)
    ```

    Other arguments, like `target` for cross-compilation, are forwarded to
    `cxx_zig_toolchain`.
    """
    mise_zig_distribution(
        name = name + "_zig",
        lock = lock,
        tool = tool,
        version = version,
        visibility = visibility,
    )
    cxx_zig_toolchain(
        name = name,
        distribution = ":{}_zig".format(name),
        visibility = visibility,
        **kwargs,
    )
