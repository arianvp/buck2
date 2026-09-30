# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

"""Downloading and unpacking a tool pinned in a mise lock file.

`mise_tool` is the building block for all of the `mise_*` toolchains: it picks
the artifact for the platform the target is configured for, downloads it with
the locked checksum and unpacks it the way mise would install it, i.e. with a
single top-level directory stripped.
"""

load("@prelude//os_lookup:defs.bzl", "Os", "OsLookup")
load("@prelude//utils:utils.bzl", "value_or")
load(":lock.bzl", "mise_lock_tool", "mise_platform_select")

MiseToolInfo = provider(
    # @unsorted-dict-items
    fields = {
        # Name of the tool in the lock file, e.g. `go` or `http:rust`.
        "tool": provider_field(str),
        # The locked version.
        "version": provider_field(str),
        # The mise platform the artifact was picked for, e.g. `linux-x64`.
        "platform": provider_field(str),
        # The unpacked tool, i.e. what mise would install into
        # `~/.local/share/mise/installs/<tool>/<version>`.
        "root": provider_field(Artifact),
        # `root`, or the directory within it given by `home` (e.g. the
        # `Contents/Home` of a macOS JDK).
        "home": provider_field(Artifact),
        # Executables within `home`, by name.
        "bins": provider_field(dict[str, RunInfo]),
        # Files or directories within `home`, by name.
        "files": provider_field(dict[str, Artifact]),
    },
)

_TAR_EXTS = [".tar", ".tar.gz", ".tgz", ".tar.xz", ".txz", ".tar.bz2", ".tbz2", ".tar.zst", ".tzst"]

def _archive_kind(url: str) -> (str, str):
    path = url.split("?")[0].split("#")[0].lower()
    for ext in _TAR_EXTS:
        if path.endswith(ext):
            return "tar", ext
    if path.endswith(".zip"):
        return "zip", ".zip"
    if path.endswith(".gz") or path.endswith(".xz") or path.endswith(".zst") or path.endswith(".bz2"):
        fail("mise: compressed single files are not supported: {}".format(url))
    return "file", ""

def _parse_checksum(tool: str, platform: str, checksum: str) -> (str, str):
    if not checksum:
        fail(
            ("mise: `{}` has no checksum for platform `{}` in the lock file. " +
             "Buck2 only downloads files with a known checksum: add it to the tool's platform in " +
             "`mise.toml` (`checksum = \"sha256:...\"`) or run `mise install` on that platform, " +
             "then `mise lock`.").format(tool, platform),
        )
    algo, _, digest = checksum.partition(":")
    if not digest:
        # A bare digest; mise defaults to sha256.
        return "sha256", algo
    if algo not in ("sha256", "sha1"):
        fail("mise: `{}` on `{}` is locked with a `{}` checksum, Buck2 can only verify sha256 or sha1".format(tool, platform, algo))
    return algo, digest

def _sh_quote(s: str) -> str:
    return "'" + s.replace("'", "'\\''") + "'"

def _ps_quote(s: str) -> str:
    return "'" + s.replace("'", "''") + "'"

def _posix_unpack_script(out: OutputArtifact, archive: Artifact, kind: str, file_name: str, strip_prefix: str | None, excludes: list[str], post_extract: list[str]) -> list:
    if kind == "tar":
        # Both GNU tar and bsdtar detect the compression when reading a file.
        unpack = ["tar -x -f \"$MISE_ARCHIVE\" " + " ".join(["--exclude=" + _sh_quote(e) for e in excludes])]
    elif kind == "zip":
        unpack = [
            "if command -v unzip >/dev/null 2>&1; then unzip -q \"$MISE_ARCHIVE\"",
            "elif tar --version 2>/dev/null | grep -q bsdtar; then tar -x -f \"$MISE_ARCHIVE\"",
            "else python3 -m zipfile -e \"$MISE_ARCHIVE\" .",
            "fi",
        ]
    else:
        unpack = [
            "mkdir -p bin",
            "cp \"$MISE_ARCHIVE\" bin/{}".format(_sh_quote(file_name)),
            "chmod +x bin/{}".format(_sh_quote(file_name)),
        ]

    if strip_prefix == None and kind != "file":
        strip = [
            # Like mise, strip the top-level directory if it is the only entry.
            "set -- $(ls -A)",
            "if [ \"$#\" -eq 1 ] && [ -d \"$1\" ] && [ ! -L \"$1\" ]; then src=\"$work/$1\"; else src=\"$work\"; fi",
        ]
    elif strip_prefix:
        strip = ["src=\"$work\"/" + _sh_quote(strip_prefix.strip("/"))]
    else:
        strip = ["src=\"$work\""]

    return [
        "set -eu",
        "root=\"$(pwd)\"",
        cmd_args(out, format = "MISE_OUT=\"$root\"/{}"),
        cmd_args(archive, format = "MISE_ARCHIVE=\"$root\"/{}"),
        "work=\"${BUCK_SCRATCH_PATH:-$MISE_OUT.scratch}\"",
        "case \"$work\" in /*) ;; *) work=\"$root/$work\" ;; esac",
        "work=\"$work/mise_unpack\"",
        "rm -rf \"$work\" \"$MISE_OUT\"",
        "mkdir -p \"$work\"",
        "cd \"$work\"",
    ] + unpack + strip + [
        "export MISE_OUT",
        "cd \"$src\"",
    ] + post_extract + [
        "cd \"$root\"",
        # `post_extract` may have installed into `$MISE_OUT` itself.
        "if [ ! -e \"$MISE_OUT\" ]; then mkdir -p \"$(dirname \"$MISE_OUT\")\"; mv \"$src\" \"$MISE_OUT\"; fi",
        "rm -rf \"$work\"",
    ]

def _windows_unpack_script(out: OutputArtifact, archive: Artifact, kind: str, file_name: str, strip_prefix: str | None, excludes: list[str]) -> list:
    if kind == "file":
        unpack = [
            "New-Item -ItemType Directory -Force -Path (Join-Path $work 'bin') | Out-Null",
            "Copy-Item -LiteralPath $archive -Destination (Join-Path (Join-Path $work 'bin') {})".format(_ps_quote(file_name)),
        ]
    else:
        # The bsdtar that ships with Windows unpacks both tarballs and zips.
        unpack = [
            "& \"$env:SystemRoot\\System32\\tar.exe\" -x -C $work -f $archive {}".format(" ".join(["--exclude=" + _ps_quote(e) for e in excludes])),
            "if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }",
        ]

    if strip_prefix == None and kind != "file":
        strip = [
            "$entries = @(Get-ChildItem -Force -LiteralPath $work)",
            "if ($entries.Count -eq 1 -and $entries[0].PSIsContainer) { $src = $entries[0].FullName } else { $src = $work }",
        ]
    elif strip_prefix:
        strip = ["$src = Join-Path $work {}".format(_ps_quote(strip_prefix.strip("/")))]
    else:
        strip = ["$src = $work"]

    return [
        "$ErrorActionPreference = 'Stop'",
        # buck-out may be a symlink (e.g. on Eden), which the bundled bsdtar
        # refuses to unpack through; resolve it like `http_archive` does.
        "function Resolve-BuckOut([string]$p) {",
        "  $link = (Get-Item -LiteralPath 'buck-out').Target",
        "  if ($link -and $p.StartsWith('buck-out')) { return Join-Path @($link)[0] $p.Substring('buck-out'.Length).TrimStart('\\', '/') }",
        "  return [System.IO.Path]::GetFullPath($p)",
        "}",
        cmd_args(out, format = "$out = Resolve-BuckOut '{}'"),
        cmd_args(archive, format = "$archive = [System.IO.Path]::GetFullPath('{}')"),
        "$scratch = $env:BUCK_SCRATCH_PATH",
        "if (-not $scratch) { $scratch = \"$out.scratch\" }",
        "$work = Join-Path (Resolve-BuckOut $scratch) 'mise_unpack'",
        "if (Test-Path -LiteralPath $work) { Remove-Item -Recurse -Force -LiteralPath $work }",
        "if (Test-Path -LiteralPath $out) { Remove-Item -Recurse -Force -LiteralPath $out }",
        "New-Item -ItemType Directory -Force -Path $work | Out-Null",
    ] + unpack + strip + [
        "New-Item -ItemType Directory -Force -Path (Split-Path -Parent $out) | Out-Null",
        "Move-Item -LiteralPath $src -Destination $out",
    ]

def _mise_tool_impl(ctx: AnalysisContext) -> list[Provider]:
    platform = ctx.attrs.platform
    if platform == None:
        details = ["{} ({})".format(p, reason) for p, reason in sorted(ctx.attrs.unusable.items())]
        fail(
            ("mise: the lock file has no prebuilt `{}@{}` for the configuration of `{}`. " +
             "Locked platforms: {}{}. Add the platform with `mise lock --platform <os>-<arch>`.").format(
                ctx.attrs.tool,
                ctx.attrs.version,
                ctx.label,
                ", ".join(sorted(ctx.attrs.artifacts.keys())),
                "; unusable: " + ", ".join(details) if details else "",
            ),
        )

    artifact = ctx.attrs.artifacts[platform]
    url = artifact["url"]
    algo, digest = _parse_checksum(ctx.attrs.tool, platform, artifact.get("checksum", ""))
    size = artifact.get("size", "")
    kind, ext = _archive_kind(url)
    exe_suffix = ".exe" if platform.startswith("windows-") else ""

    # Like mise, name a bare executable after the tool (`github:jqlang/jq` -> `jq`).
    file_name = value_or(ctx.attrs.file_name, ctx.attrs.tool.split(":")[-1].split("/")[-1] + exe_suffix)

    digest_config = ctx.actions.digest_config()
    prefer_local = not ((algo == "sha1" and digest_config.allows_sha1()) or (algo == "sha256" and digest_config.allows_sha256()))

    archive = ctx.actions.declare_output("download" + ext if kind != "file" else "download", has_content_based_path = False)
    ctx.actions.download_file(
        archive.as_output(),
        url,
        sha1 = digest if algo == "sha1" else None,
        sha256 = digest if algo == "sha256" else None,
        size_bytes = int(size) if size else None,
        is_executable = kind == "file",
        has_content_based_path = False,
    )

    out = ctx.actions.declare_output(ctx.label.name, dir = True, has_content_based_path = False)
    exec_is_windows = ctx.attrs._exec_os_type[OsLookup].os == Os("windows")
    if exec_is_windows:
        if ctx.attrs.post_extract:
            fail("mise: `{}` needs `post_extract` steps, which only run on POSIX execution platforms".format(ctx.attrs.tool))
        lines = _windows_unpack_script(out.as_output(), archive, kind, file_name, ctx.attrs.strip_prefix, ctx.attrs.excludes)
        script_ext = "ps1"
        interpreter = ["powershell", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]
    else:
        lines = _posix_unpack_script(out.as_output(), archive, kind, file_name, ctx.attrs.strip_prefix, ctx.attrs.excludes, ctx.attrs.post_extract)
        script_ext = "sh"
        interpreter = ["/bin/sh"]

    script, _ = ctx.actions.write(
        "unpack." + script_ext,
        lines,
        is_executable = True,
        allow_args = True,
        has_content_based_path = False,
    )
    ctx.actions.run(
        cmd_args(interpreter + [script], hidden = [archive, out.as_output()]),
        category = "mise_unpack",
        identifier = "{}@{}".format(ctx.attrs.tool, ctx.attrs.version),
        prefer_local = prefer_local,
    )

    home = out.project(ctx.attrs.home) if ctx.attrs.home else out

    bins = {}
    sub_targets = {}
    for name, path in ctx.attrs.bins.items():
        if path.endswith("{exe}"):
            path = path.removesuffix("{exe}") + exe_suffix
        exe = home.project(path)
        bins[name] = RunInfo(args = cmd_args(exe, hidden = out))
        sub_targets[name] = [DefaultInfo(default_output = exe, other_outputs = [out]), bins[name]]

    files = {}
    for name, path in ctx.attrs.files.items():
        files[name] = home.project(path) if path else home
        sub_targets[name] = [DefaultInfo(default_output = files[name], other_outputs = [out])]

    if "home" not in sub_targets:
        sub_targets["home"] = [DefaultInfo(default_output = home, other_outputs = [out])]

    return [
        DefaultInfo(default_output = out, sub_targets = sub_targets),
        MiseToolInfo(
            tool = ctx.attrs.tool,
            version = ctx.attrs.version,
            platform = platform,
            root = out,
            home = home,
            bins = bins,
            files = files,
        ),
    ]

mise_tool_rule = rule(
    impl = _mise_tool_impl,
    doc = "Downloads and unpacks one tool from a mise lock file. Use the `mise_tool` macro, which fills in `artifacts` and `platform` from the lock file.",
    attrs = {
        "artifacts": attrs.dict(attrs.string(), attrs.dict(attrs.string(), attrs.string()), doc = "mise platform -> {url, checksum, size}"),
        "bins": attrs.dict(attrs.string(), attrs.string(), default = {}, doc = "Executables, relative to `home`, exposed as sub-targets with `RunInfo`. A trailing `{exe}` becomes `.exe` on Windows."),
        "excludes": attrs.list(attrs.string(), default = [], doc = "Patterns of archive members not to unpack."),
        "file_name": attrs.option(attrs.string(), default = None, doc = "For artifacts that are a single executable: its name under `bin/`. Defaults to the name of the tool."),
        "files": attrs.dict(attrs.string(), attrs.string(), default = {}, doc = "Files or directories, relative to `home`, exposed as sub-targets."),
        "home": attrs.option(attrs.string(), default = None, doc = "Directory within the unpacked tool that `bins` and `files` are relative to."),
        "platform": attrs.option(attrs.string(), doc = "The mise platform to use, usually a `select()` from `mise_platform_select`."),
        "post_extract": attrs.list(attrs.string(), default = [], doc = "POSIX shell lines to run in the unpacked directory. `$MISE_OUT` is the absolute output path; if they create it, it is used as the output instead of the unpacked directory."),
        "strip_prefix": attrs.option(attrs.string(), default = None, doc = "Directory within the archive to use as the root. By default a single top-level directory is stripped, like mise does."),
        "tool": attrs.string(),
        "unusable": attrs.dict(attrs.string(), attrs.string(), default = {}),
        "version": attrs.string(),
        "_exec_os_type": attrs.default_only(attrs.exec_dep(default = "prelude//os_lookup/targets:os_lookup", providers = [OsLookup])),
    },
)

def mise_tool(
        name: str,
        lock: dict,
        tool: str,
        version: str | None = None,
        **kwargs):
    """
    Download and unpack `tool` as pinned in the mise lock file `lock`.

    The artifact is picked for the platform the target is configured for. When
    used as an `exec_dep` (as toolchains do) that is the execution platform.

    ```python
    load("@prelude//toolchains/mise:defs.bzl", "mise_tool")
    load(":mise.lock.toml", mise_lock = "value")

    mise_tool(
        name = "node",
        lock = mise_lock,
        tool = "node",
        bins = {"node": "bin/node"},
    )
    ```

    `bins` and `files` become sub-targets (`:node[node]`); the unpacked
    directory is the default output and the `MiseToolInfo` provider carries all
    of it.
    """
    locked = mise_lock_tool(lock, tool, version)
    mise_tool_rule(
        name = name,
        tool = locked.tool,
        version = locked.version,
        artifacts = locked.artifacts,
        unusable = locked.unusable,
        platform = mise_platform_select({p: p for p in locked.artifacts.keys()}),
        **kwargs,
    )
    return locked
