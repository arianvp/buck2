# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@prelude//toolchains/go:go_bootstrap_toolchain.bzl", "go_bootstrap_distr", "go_bootstrap_toolchain")
load("@prelude//toolchains/go:go_toolchain.bzl", "go_distr", "go_toolchain")
load(":archive.bzl", "mise_tool")
load(":lock.bzl", "mise_platform_map", "mise_platform_select")

_GOOS = {
    "freebsd": "freebsd",
    "linux": "linux",
    "macos": "darwin",
    "windows": "windows",
}

_GOARCH = {
    "arm": "arm",
    "arm64": "arm64",
    "armv7": "arm",
    "riscv64": "riscv64",
    "x64": "amd64",
    "x86": "386",
}

# GOOS/GOARCH of the target platform.
_TARGET_GOOS = select({
    "prelude//os:freebsd": "freebsd",
    "prelude//os:linux": "linux",
    "prelude//os:macos": "darwin",
    "prelude//os:windows": "windows",
})

_TARGET_GOARCH = select({
    "prelude//cpu:arm32": "arm",
    "prelude//cpu:arm64": "arm64",
    "prelude//cpu:riscv64": "riscv64",
    "prelude//cpu:x86_32": "386",
    "prelude//cpu:x86_64": "amd64",
})

def mise_go_toolchain(
        name: str,
        lock: dict,
        tool: str = "go",
        version: str | None = None,
        bootstrap_name: str | None = None,
        env_go_os = _TARGET_GOOS,
        env_go_arch = _TARGET_GOARCH,
        visibility: list[str] = ["PUBLIC"],
        **kwargs):
    """
    A Go toolchain using the Go distribution pinned in a mise lock file.

    Also defines the `go_bootstrap` toolchain (named `bootstrap_name`,
    `<name>_bootstrap` by default) from the same distribution. Pass
    `bootstrap_name = ""` to skip it.

    ```python
    load("@prelude//toolchains/mise:defs.bzl", "mise_go_toolchain")
    load(":mise.lock.toml", mise_lock = "value")

    mise_go_toolchain(name = "go", lock = mise_lock)  # also defines :go_bootstrap
    ```

    Other arguments are forwarded to `go_toolchain`.
    """
    locked = mise_tool(
        name = name + "_mise",
        lock = lock,
        tool = tool,
        version = version,
        visibility = visibility,
    )
    go_os_arch = mise_platform_select(mise_platform_map(
        locked.artifacts.keys(),
        lambda os, arch, _libc: (_GOOS[os], _GOARCH[arch]),
        # Keep the attribute valid on platforms without a locked artifact so
        # that the `mise_tool` target reports the missing platform.
    ), default = ("", ""))
    go_version = locked.version.removeprefix("go")

    go_distr(
        name = name + "_distr",
        go_os_arch = go_os_arch,
        go_root = ":{}_mise".format(name),
        version = go_version,
    )
    go_toolchain(
        name = name,
        env_go_arch = env_go_arch,
        env_go_os = env_go_os,
        go_distr = ":{}_distr".format(name),
        visibility = visibility,
        **kwargs,
    )

    if bootstrap_name == None:
        bootstrap_name = name + "_bootstrap"
    if bootstrap_name:
        go_bootstrap_distr(
            name = bootstrap_name + "_distr",
            go_os_arch = go_os_arch,
            go_root = ":{}_mise".format(name),
        )
        go_bootstrap_toolchain(
            name = bootstrap_name,
            env_go_arch = env_go_arch,
            env_go_os = env_go_os,
            go_bootstrap_distr = ":{}_distr".format(bootstrap_name),
            visibility = visibility,
        )
