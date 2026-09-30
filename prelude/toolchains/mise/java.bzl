# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load(
    "@prelude//toolchains:java.bzl",
    "java_test_toolchain",
    "javacd_toolchain",
    "system_java_bootstrap_toolchain",
    "system_prebuilt_jar_bootstrap_toolchain",
)
load("@prelude//toolchains:kotlin.bzl", "kotlincd_toolchain", "system_kotlin_bootstrap_toolchain")
load(":archive.bzl", "mise_tool")

# macOS JDKs are bundles with the JDK proper in `Contents/Home`; use that as
# the root so that the layout is the same everywhere.
_MACOS_BUNDLE = [
    "if [ ! -d bin ] && [ -d Contents/Home ]; then mv Contents/Home \"$MISE_OUT\"; fi",
]

def mise_jdk(
        name: str,
        lock: dict,
        tool: str = "java",
        version: str | None = None,
        visibility: list[str] = ["PUBLIC"]):
    """
    The JDK pinned in a mise lock file.

    The tools are sub-targets that plug into the toolchains in
    `@prelude//toolchains:java.bzl`: `[java]`, `[javac]`, `[jar]`, `[jlink]`,
    `[jmod]` and `[jrt-fs.jar]`; `[home]` is the `JAVA_HOME`.
    """
    return mise_tool(
        name = name,
        lock = lock,
        tool = tool,
        version = version,
        post_extract = _MACOS_BUNDLE,
        bins = {
            b: "bin/{}{{exe}}".format(b)
            for b in ["jar", "java", "javac", "javadoc", "jlink", "jmod"]
        },
        files = {
            "jrt-fs.jar": "lib/jrt-fs.jar",
        },
        visibility = visibility,
    )

def mise_java_toolchains(
        lock: dict,
        tool: str = "java",
        version: str | None = None,
        kotlin: bool = True,
        visibility: list[str] = ["PUBLIC"]):
    """
    The JVM toolchains (Java, Kotlin, prebuilt jars) using the JDK pinned in
    a mise lock file.

    Defines the same toolchains as `system_demo_toolchains` does for the JVM,
    with the same names: `java`, `java_bootstrap`, `java_for_android`,
    `java_for_host_test`, `java_test`, `prebuilt_jar`,
    `prebuilt_jar_bootstrap`, `prebuilt_jar_bootstrap_no_snapshot` and, with
    `kotlin`, `kotlin`, `kotlin_bootstrap` and `kotlin_for_android`. The JDK
    itself is `:jdk`.

    ```python
    load("@prelude//toolchains/mise:defs.bzl", "mise_java_toolchains")
    load(":mise.lock.toml", mise_lock = "value")

    mise_java_toolchains(lock = mise_lock)
    ```

    The Kotlin compiler comes from the prelude, like in the system toolchains;
    only the JDK it runs on comes from the lock file.
    """
    mise_jdk(
        name = "jdk",
        lock = lock,
        tool = tool,
        version = version,
        visibility = visibility,
    )
    jdk = {
        "jar": ":jdk[jar]",
        "java": ":jdk[java]",
        "javac": ":jdk[javac]",
        "jlink": ":jdk[jlink]",
        "jmod": ":jdk[jmod]",
        "jrt_fs_jar": ":jdk[jrt-fs.jar]",
    }

    javacd_toolchain(name = "java", visibility = visibility, **jdk)
    javacd_toolchain(name = "java_for_android", visibility = visibility, **jdk)
    javacd_toolchain(name = "java_for_host_test", java_for_tests = ":jdk[java]", visibility = visibility, **jdk)
    system_java_bootstrap_toolchain(
        name = "java_bootstrap",
        visibility = visibility,
        **{k: v for k, v in jdk.items() if k != "jar"},
    )
    java_test_toolchain(name = "java_test", visibility = visibility)

    for name in ["prebuilt_jar", "prebuilt_jar_bootstrap", "prebuilt_jar_bootstrap_no_snapshot"]:
        system_prebuilt_jar_bootstrap_toolchain(name = name, java = ":jdk[java]", visibility = visibility)

    if kotlin:
        kotlincd_toolchain(name = "kotlin", java_binary_for_kotlincd = ":jdk[java]", visibility = visibility)
        kotlincd_toolchain(name = "kotlin_for_android", java_binary_for_kotlincd = ":jdk[java]", visibility = visibility)
        system_kotlin_bootstrap_toolchain(name = "kotlin_bootstrap", visibility = visibility)
