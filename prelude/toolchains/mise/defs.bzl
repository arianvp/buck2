# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

"""Hermetic toolchains from a [mise](https://mise.jdx.dev) lock file.

mise pins tool versions in `mise.toml` and, with `mise lock`, records the
download URL and checksum of every tool for every platform in `mise.lock`.
The rules here download those same artifacts as build actions, so the
toolchains don't depend on anything installed on the machine, work with
remote execution and stay in sync with the tools developers use through mise.

## Setup

Buck2 can only `load()` TOML files whose name ends in `.toml`. Keep the lock
file as `mise.lock.toml` and make `mise.lock` a symlink to it; mise writes
through the symlink:

```sh
mise lock                          # creates mise.lock
mv mise.lock mise.lock.toml
ln -s mise.lock.toml mise.lock
```

(The symlink must point this way round: Buck2 does not notice changes to the
target of a symlinked `.toml` file, so loading a `mise.lock.toml` symlink gives
stale results after `mise lock` until the daemon restarts.)

Lock every platform you build on or execute on, e.g. in `mise.toml`:

```toml
[settings]
lockfile = true
lockfile_platforms = ["linux-x64", "linux-arm64", "macos-arm64", "windows-x64"]
```

Then define the toolchains, e.g. in `toolchains//BUCK`:

```python
load(
    "@prelude//toolchains/mise:defs.bzl",
    "mise_cxx_zig_toolchain",
    "mise_erlang_toolchain",
    "mise_go_toolchain",
    "mise_java_toolchains",
    "mise_python_toolchain",
    "mise_rust_toolchain",
)
load(":mise.lock.toml", mise_lock = "value")

mise_cxx_zig_toolchain(name = "cxx", lock = mise_lock)
mise_erlang_toolchain(name = "erlang-default", lock = mise_lock)
mise_go_toolchain(name = "go", lock = mise_lock)                # and :go_bootstrap
mise_java_toolchains(lock = mise_lock)                          # :java, :kotlin, ...
mise_python_toolchain(name = "python", lock = mise_lock)        # and :python_bootstrap
mise_rust_toolchain(name = "rust", lock = mise_lock, default_edition = "2021")
```

Each toolchain picks the artifact for its execution platform through the
`prelude//os`, `prelude//cpu` and `prelude//abi:musl` constraints, so the
execution platforms need to set them (`prelude//platforms:default` does).

## Supported toolchains

| Toolchain | Macro | mise tool |
|---|---|---|
| C/C++ | `mise_cxx_zig_toolchain` | `zig` (`core:zig`) |
| Erlang | `mise_erlang_toolchain` | `erlang` (`core:erlang`), Linux (glibc) and macOS |
| Go | `mise_go_toolchain` | `go` (`core:go`) |
| Java, Kotlin | `mise_java_toolchains`, `mise_jdk` | `java` (`core:java`) |
| Python | `mise_python_toolchain` | `python` (`core:python`) |
| Rust | `mise_rust_toolchain` | the standalone installers through `http:` (`core:rust` uses rustup and locks no download) |

`mise_tool` downloads and unpacks any other tool (e.g. `node`) and exposes its
executables as sub-targets.

Haskell, OCaml and C# are not covered: mise doesn't lock a prebuilt that runs
from any location for them (GHC and OCaml come from ghcup, opam or conda, which
install with absolute paths; the prelude's C# rules need the .NET Framework's
`csc.exe`).
"""

load(":archive.bzl", _MiseToolInfo = "MiseToolInfo", _mise_tool = "mise_tool")
load(":cxx.bzl", _mise_cxx_zig_toolchain = "mise_cxx_zig_toolchain", _mise_zig_distribution = "mise_zig_distribution")
load(":erlang.bzl", _mise_erlang_toolchain = "mise_erlang_toolchain")
load(":go.bzl", _mise_go_toolchain = "mise_go_toolchain")
load(":java.bzl", _mise_java_toolchains = "mise_java_toolchains", _mise_jdk = "mise_jdk")
load(":lock.bzl", _mise_lock_tool = "mise_lock_tool", _mise_platform_select = "mise_platform_select")
load(":python.bzl", _mise_python_toolchain = "mise_python_toolchain")
load(":rust.bzl", _mise_rust_toolchain = "mise_rust_toolchain")

MiseToolInfo = _MiseToolInfo
mise_cxx_zig_toolchain = _mise_cxx_zig_toolchain
mise_erlang_toolchain = _mise_erlang_toolchain
mise_go_toolchain = _mise_go_toolchain
mise_java_toolchains = _mise_java_toolchains
mise_jdk = _mise_jdk
mise_lock_tool = _mise_lock_tool
mise_platform_select = _mise_platform_select
mise_python_toolchain = _mise_python_toolchain
mise_rust_toolchain = _mise_rust_toolchain
mise_tool = _mise_tool
mise_zig_distribution = _mise_zig_distribution
