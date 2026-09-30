# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@prelude//:prelude.bzl", "native")
load("@prelude//toolchains:python.bzl", "python_bootstrap_toolchain", "python_toolchain")
load(":archive.bzl", "mise_tool")
load(":lock.bzl", "mise_lock_tool")

def mise_python_toolchain(
        name: str,
        lock: dict,
        tool: str = "python",
        version: str | None = None,
        bootstrap_name: str | None = None,
        visibility: list[str] = ["PUBLIC"],
        **kwargs):
    """
    A Python toolchain using the CPython pinned in a mise lock file.

    mise's `core:python` backend locks the
    [python-build-standalone](https://github.com/astral-sh/python-build-standalone)
    builds, which run from any location.

    Also defines the `python_bootstrap` toolchain (named `bootstrap_name`,
    `<name>_bootstrap` by default) from the same interpreter. Pass
    `bootstrap_name = ""` to skip it.

    ```python
    load("@prelude//toolchains/mise:defs.bzl", "mise_python_toolchain")
    load(":mise.lock.toml", mise_lock = "value")

    mise_python_toolchain(name = "python", lock = mise_lock)  # also defines :python_bootstrap
    ```

    Other arguments, like `type_checker`, are forwarded to `python_toolchain`.
    """
    archive = name + "_mise"

    # e.g. `3.13.6`; the headers live in `include/python3.13`.
    python_version = mise_lock_tool(lock, tool, version).version.split("-")[-1]
    major_minor = ".".join(python_version.split(".")[:2])
    locked = mise_tool(
        name = archive,
        lock = lock,
        tool = tool,
        version = version,
        bins = {
            "python": select({"DEFAULT": "bin/python3", "prelude//os:windows": "python.exe"}),
        },
        files = {
            "include": select({
                "DEFAULT": "include/python" + major_minor,
                "prelude//os:windows": "include",
            }),
            "lib": select({"DEFAULT": "lib", "prelude//os:windows": "libs"}),
        },
        visibility = visibility,
    )
    interpreter = ":{}[python]".format(archive)

    if bootstrap_name == None:
        bootstrap_name = name + "_bootstrap"
    if bootstrap_name:
        python_bootstrap_toolchain(
            name = bootstrap_name,
            interpreter = interpreter,
            visibility = visibility,
        )

    native.genrule(
        name = name + "_libpython_symbols",
        out = "linker_args",
        cmd = '$(exe_target prelude//python/tools:gather_libpython_symbols) "$OUT"',
    )

    python_toolchain(
        name = name,
        interpreter = interpreter,
        extension_linker_flags = select({
            "DEFAULT": [
                "-L$(location :{}[lib])".format(archive),
                "@$(location :{}_libpython_symbols)".format(name),
            ],
            "prelude//os:windows": ["/LIBPATH:$(location :{}[lib])".format(archive)],
        }),
        visibility = visibility,
        **kwargs,
    )
    return locked
