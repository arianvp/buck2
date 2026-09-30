# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

"""Reading [mise](https://mise.jdx.dev) lock files (`mise.lock`).

A lock file lists, for every tool, the resolved version and, for every
platform it was locked for, the download URL and checksum of the prebuilt
artifact:

```toml
[[tools.go]]
version = "1.24.4"
backend = "core:go"

[tools.go."platforms.linux-x64"]
checksum = "sha256:77e5da33bb72aeaef1ba4418b6fe511bc4d041873cbf82e5aa6318740df98717"
url = "https://dl.google.com/go/go1.24.4.linux-amd64.tar.gz"
```

Buck2 can only `load()` TOML files that end in `.toml`, so keep the lock file
as `mise.lock.toml` with a `mise.lock -> mise.lock.toml` symlink for mise
(see `defs.bzl`):

```python
load(":mise.lock.toml", mise_lock = "value")
```

The functions in this file turn that value into what the `mise_*` rules need:
a map from mise platform name (`linux-x64`, `macos-arm64`, ...) to the
artifact, and a `select()` that picks a mise platform name from the
configuration a target is built in.
"""

# mise platform components -> prelude config settings.
_MISE_OS = {
    "freebsd": "prelude//os:freebsd",
    "linux": "prelude//os:linux",
    "macos": "prelude//os:macos",
    "windows": "prelude//os:windows",
}

_MISE_ARCH = {
    "arm": "prelude//cpu:arm32",
    "arm64": "prelude//cpu:arm64",
    "armv7": "prelude//cpu:arm32",
    "riscv64": "prelude//cpu:riscv64",
    "x64": "prelude//cpu:x86_64",
    "x86": "prelude//cpu:x86_32",
}

# Qualifiers mise appends to a platform name to pick a libc. Unqualified
# names are the glibc (or the only) build for that platform.
_MISE_LIBC = {
    "gnu": None,
    "musl": "prelude//abi:musl",
}

MiseLockedTool = record(
    # The key of the tool in the lock file, e.g. `go` or `http:rust`.
    tool = field(str),
    version = field(str),
    backend = field(str | None),
    # mise platform name -> {"url": str, "checksum": str, "size": str}. Only
    # platforms with a prebuilt artifact are present.
    artifacts = field(dict[str, dict[str, str]]),
    # Platforms that are locked but have no usable prebuilt artifact, mapped
    # to the reason.
    unusable = field(dict[str, str]),
)

def _find_tool_key(tools: dict, tool: str) -> str:
    if tool in tools:
        return tool

    # Allow `rust` to find `http:rust`, `kotlin` to find `github:JetBrains/kotlin`, ...
    candidates = [
        key
        for key in tools.keys()
        if key.split(":")[-1] == tool or key.split(":")[-1].split("/")[-1] == tool
    ]
    if len(candidates) == 1:
        return candidates[0]
    if len(candidates) > 1:
        fail("mise: `{}` is ambiguous in the lock file, it matches {}. Pass the full key as `tool`.".format(
            tool,
            ", ".join(["`{}`".format(c) for c in candidates]),
        ))
    fail("mise: tool `{}` is not in the lock file. Locked tools: {}".format(
        tool,
        ", ".join(["`{}`".format(t) for t in sorted(tools.keys())]) or "none",
    ))

def _platform_entries(entry: dict) -> dict[str, dict]:
    platforms = {}

    # `lockfile_version = 3` spells platforms as `"platforms.linux-x64" = {...}`.
    for key, value in entry.items():
        if key.startswith("platforms.") and type(value) == type({}):
            platforms[key.removeprefix("platforms.")] = value

    # Earlier versions nest them: `[tools.go.platforms.linux-x64]`.
    nested = entry.get("platforms")
    if type(nested) == type({}):
        for key, value in nested.items():
            if type(value) == type({}):
                platforms[key] = value
    return platforms

def mise_lock_tool(lock: dict, tool: str, version: str | None = None) -> MiseLockedTool:
    """
    Find `tool` in the lock file `lock` (as loaded from TOML).

    `version` is only needed when the lock file pins several versions of the
    same tool.
    """
    if type(lock) != type({}):
        fail("mise: `lock` must be the value loaded from the lock file, e.g. `load(\":mise.lock.toml\", mise_lock = \"value\")`, got `{}`".format(type(lock)))
    tools = lock.get("tools", {})
    key = _find_tool_key(tools, tool)

    entries = tools[key]
    if type(entries) == type({}):
        entries = [entries]

    if version != None:
        entries = [e for e in entries if e.get("version") == version]
        if not entries:
            fail("mise: `{}@{}` is not in the lock file. Locked versions: {}".format(
                key,
                version,
                ", ".join([str(e.get("version")) for e in tools[key]]),
            ))
    versions = {e.get("version"): None for e in entries}.keys()
    if len(versions) != 1:
        fail("mise: the lock file pins several versions of `{}` ({}), pass `version` to pick one".format(
            key,
            ", ".join([str(v) for v in versions]),
        ))

    artifacts = {}
    unusable = {}

    # mise splits entries of the same version when their options differ per
    # platform (e.g. erlang), so merge them.
    for entry in entries:
        for platform, info in _platform_entries(entry).items():
            url = info.get("url")
            if info.get("install") == "source":
                unusable[platform] = "mise builds it from source on this platform"
            elif not url:
                unusable[platform] = "the lock file has no download URL for it"
            else:
                artifacts[platform] = {
                    "checksum": info.get("checksum", ""),
                    "size": str(info.get("size", "")),
                    "url": url,
                }
                unusable.pop(platform, None)

    if not artifacts:
        fail(
            ("mise: `{}@{}` has no prebuilt artifacts in the lock file (backend `{}`). " +
             "Some backends (e.g. `core:rust`, which uses rustup) install without a download URL; " +
             "use a backend that downloads a prebuilt archive, e.g. `http:` or `github:`, and run `mise lock`.").format(
                key,
                versions[0],
                entries[0].get("backend"),
            ),
        )

    return MiseLockedTool(
        tool = key,
        version = versions[0],
        backend = entries[0].get("backend"),
        artifacts = artifacts,
        unusable = unusable,
    )

def parse_mise_platform(platform: str) -> (str, str, str | None) | None:
    """
    Split a mise platform name like `linux-x64-musl` into `(os, arch, libc)`.

    Returns `None` for platform names that don't map onto prelude constraints.
    """
    parts = platform.split("-")
    if len(parts) < 2 or len(parts) > 3:
        return None
    os, arch = parts[0], parts[1]
    libc = parts[2] if len(parts) == 3 else None
    if os not in _MISE_OS or arch not in _MISE_ARCH:
        return None
    if libc != None and libc not in _MISE_LIBC:
        return None
    if libc == "gnu":
        libc = None
    return (os, arch, libc)

def mise_platform_select(values: dict[str, typing.Any], default = None):
    """
    Build a `select()` that picks `values[mise_platform]` for the mise
    platform matching the configuration, and `default` when none matches.

    `values` is keyed by mise platform name (e.g. `linux-x64`). A `-musl`
    platform is chosen when the configuration has the `prelude//abi:musl`
    constraint, otherwise the unqualified (glibc) one.
    """
    by_os = {}
    for platform, value in values.items():
        parsed = parse_mise_platform(platform)
        if parsed == None:
            continue
        os, arch, libc = parsed
        by_arch = by_os.setdefault(_MISE_OS[os], {})
        by_libc = by_arch.setdefault(_MISE_ARCH[arch], {})
        by_libc[libc] = value

    def libc_select(by_libc):
        glibc = by_libc.get(None, default)
        if "musl" not in by_libc:
            return glibc
        return select({
            _MISE_LIBC["musl"]: by_libc["musl"],
            "DEFAULT": glibc,
        })

    return select(
        {
            os_setting: select(
                {cpu: libc_select(by_libc) for cpu, by_libc in by_arch.items()} |
                {"DEFAULT": default},
            )
            for os_setting, by_arch in by_os.items()
        } | {"DEFAULT": default},
    )

def mise_platform_map(platforms: list[str], f) -> dict[str, typing.Any]:
    """
    Apply `f(os, arch, libc)` to each mise platform name that maps onto
    prelude constraints. Useful with `mise_platform_select`.
    """
    result = {}
    for platform in platforms:
        parsed = parse_mise_platform(platform)
        if parsed != None:
            result[platform] = f(parsed[0], parsed[1], parsed[2])
    return result
