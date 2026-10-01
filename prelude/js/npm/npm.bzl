# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Lockfile-driven npm builds, without generating BUCK files for packages.
# `lockfile` may be an npm package-lock.json (v2/v3) or a bun text lockfile
# (bun.lock); the latter is first converted to the package-lock.json shape.
#
# - `npm_download` is the only step that touches the network. It reads
#   package-lock.json and downloads every tarball for the target platform,
#   verifying each one against its `integrity` hash (usually SHA-512, which
#   buck2's own `download_file` cannot check). Output files are named after
#   their integrity hash, so each package has a stable path in the output.
# - `npm_node_modules` extracts each tarball in its own action (its inputs are
#   just that tarball, node and the extract script; the tarball is checked
#   against its `integrity` hash again) and
#   assembles node_modules exactly as laid out in the lockfile, including
#   nested packages, bundled dependencies and `.bin` links. The default
#   output includes dev dependencies; the `[prod]` sub-target omits them.
# - `npm_build` runs an npm script (e.g. `vite build`) offline against that
#   node_modules and captures an output directory.
#
# Install scripts (`preinstall`/`install`/`postinstall`) are not run, like
# `npm ci --ignore-scripts`.

load(":toolchain.bzl", "NodeToolchainInfo")

NpmTarballsInfo = provider(
    fields = {
        # The lockfile the tarballs were downloaded for, in package-lock.json
        # (v2/v3) shape (bun.lock is converted).
        "lockfile": provider_field(Artifact),
        # Directory of package tarballs, named by `tarball_name(integrity)`.
        "tarballs": provider_field(Artifact),
    },
)

NpmNodeModulesInfo = provider(
    fields = {
        "node_modules": provider_field(Artifact),
    },
)

def _matches_platform(values: list[str] | None, current: str) -> bool:
    # npm semantics for `os`/`cpu`: a list of allowed values, or "!value" to deny.
    if not values:
        return True
    if "!" + current in values:
        return False
    allowed = [v for v in values if not v.startswith("!")]
    return not allowed or current in allowed

def _tarball_name(integrity: str) -> str:
    # The name of a package's file in npm_download's output: the download
    # manifest and the extracts' projections both use it.
    token = integrity.strip().split(" ")[0]
    return token.replace("/", "_").replace("+", "-").rstrip("=") + ".tgz"

def _lock_packages(lock: dict, node_os: str, node_cpu: str, include_dev: bool) -> list[(str, dict)]:
    """Returns the (install path, lock entry) pairs to install on this platform."""
    if lock.get("lockfileVersion", 1) < 2:
        fail("npm: package-lock.json must be lockfileVersion 2 or 3 (npm 7+); regenerate it with a recent npm")

    packages = []
    for install_path, entry in lock.get("packages", {}).items():
        if install_path == "":
            continue  # the root project itself
        if entry.get("link") or not install_path.startswith("node_modules/"):
            fail("npm: workspaces and linked packages are not supported yet (`{}`)".format(install_path))
        if entry.get("dev") and not include_dev:
            continue
        if not (_matches_platform(entry.get("os"), node_os) and _matches_platform(entry.get("cpu"), node_cpu)):
            if entry.get("optional"):
                continue
            fail("npm: `{}` does not support {}/{}".format(install_path, node_os, node_cpu))
        if entry.get("inBundle"):
            # A bundled dependency (`bundleDependencies`) ships inside its
            # parent's tarball: nothing to download or extract, but its bins
            # are still linked.
            pass
        elif not entry.get("resolved") or not entry.get("integrity"):
            fail("npm: `{}` has no `resolved` URL and `integrity` hash (git and file dependencies are not supported)".format(install_path))
        packages.append((install_path, entry))
    return packages

def _normalized_lockfile(ctx: AnalysisContext, node: NodeToolchainInfo, tool: Artifact) -> Artifact:
    """Returns a package-lock.json (v2/v3) shaped lockfile for `ctx.attrs.lockfile`."""
    lockfile = ctx.attrs.lockfile
    if lockfile.basename == "bun.lockb":
        fail("npm: the binary bun.lockb is not supported; switch to the text bun.lock (`bun install --save-text-lockfile`)")
    if lockfile.basename != "bun.lock":
        return lockfile

    converted = ctx.actions.declare_output("bun_lock.json")
    ctx.actions.run(
        [node.node, tool, "bun-lock", lockfile, converted.as_output()],
        category = "bun_lock",
    )
    return converted

# ---------------------------------------------------------------------------
# npm_download

def npm_download_impl(ctx: AnalysisContext) -> list[Provider]:
    node = ctx.attrs._node_toolchain[NodeToolchainInfo]
    tool = ctx.attrs._npm_tool[DefaultInfo].default_outputs[0]

    # Not content-based: packages are projected out of this directory, and a
    # path that hashes the whole directory would change every extract action
    # whenever any single package changes.
    lockfile = _normalized_lockfile(ctx, node, tool)
    tarballs = ctx.actions.declare_output("tarballs", dir = True, has_content_based_path = False)
    ctx.actions.dynamic_output_new(
        _download(
            lockfile = lockfile,
            node = node,
            fetch_tool = ctx.attrs._npm_fetch[DefaultInfo].default_outputs[0],
            out = tarballs.as_output(),
        ),
    )
    return [
        DefaultInfo(default_output = tarballs),
        NpmTarballsInfo(lockfile = lockfile, tarballs = tarballs),
    ]

def _download_impl(
        actions: AnalysisActions,
        lockfile: ArtifactValue,
        node: NodeToolchainInfo,
        fetch_tool: Artifact,
        out: OutputArtifact) -> list[Provider]:
    # Download dev dependencies too, so one download serves both dev and
    # production node_modules.
    manifest = {}
    for _, entry in _lock_packages(lockfile.read_json(), node.os, node.cpu, include_dev = True):
        if entry.get("inBundle"):
            continue
        name = _tarball_name(entry["integrity"])
        manifest[name] = {"integrity": entry["integrity"], "name": name, "url": entry["resolved"]}

    manifest_file = actions.write_json("fetch_manifest.json", [manifest[k] for k in sorted(manifest)])
    actions.run(
        [node.node, fetch_tool, manifest_file, out],
        env = {"NODE_USE_ENV_PROXY": "1"},
        category = "npm_fetch",
        # Needs network access; the integrity hashes make the result deterministic.
        local_only = True,
        # Incremental: keep tarballs from the previous run and only download
        # what changed. The tool removes files the lockfile no longer lists.
        no_outputs_cleanup = True,
        allow_cache_upload = True,
    )
    return []

_download = dynamic_actions(
    impl = _download_impl,
    attrs = {
        "fetch_tool": dynattrs.value(Artifact),
        "lockfile": dynattrs.artifact_value(),
        "node": dynattrs.value(NodeToolchainInfo),
        "out": dynattrs.output(),
    },
)

# ---------------------------------------------------------------------------
# npm_node_modules

def npm_node_modules_impl(ctx: AnalysisContext) -> list[Provider]:
    node = ctx.attrs._node_toolchain[NodeToolchainInfo]
    tool = ctx.attrs._npm_tool[DefaultInfo].default_outputs[0]
    tarballs = ctx.attrs.tarballs[NpmTarballsInfo]

    # Both trees come from one dynamic action so they share extract actions;
    # buck2 only runs the assembly for the one(s) actually requested.
    node_modules = ctx.actions.declare_output("node_modules", dir = True)
    prod_node_modules = ctx.actions.declare_output("prod/node_modules", dir = True)
    ctx.actions.dynamic_output_new(
        _node_modules(
            lockfile = tarballs.lockfile,
            tarballs = tarballs.tarballs,
            node = node,
            tool = tool,
            extract_tool = ctx.attrs._npm_extract[DefaultInfo].default_outputs[0],
            out = node_modules.as_output(),
            prod_out = prod_node_modules.as_output(),
        ),
    )
    return [
        DefaultInfo(
            default_output = node_modules,
            sub_targets = {
                "prod": [
                    DefaultInfo(default_output = prod_node_modules),
                    NpmNodeModulesInfo(node_modules = prod_node_modules),
                ],
            },
        ),
        NpmNodeModulesInfo(node_modules = node_modules),
    ]

def _node_modules_impl(
        actions: AnalysisActions,
        lockfile: ArtifactValue,
        tarballs: Artifact,
        node: NodeToolchainInfo,
        tool: Artifact,
        extract_tool: Artifact,
        out: OutputArtifact,
        prod_out: OutputArtifact) -> list[Provider]:
    lock = lockfile.read_json()
    extracted = {}
    packages = []
    for install_path, entry in _lock_packages(lock, node.os, node.cpu, include_dev = True):
        pkg_dir = None
        if not entry.get("inBundle"):
            name = _tarball_name(entry["integrity"])
            if name not in extracted:
                # One action per distinct tarball: its inputs are that
                # tarball, node and the small extract script.
                extracted[name] = actions.declare_output("extracted/" + name.removesuffix(".tgz"), dir = True)
                actions.run(
                    [node.node, extract_tool, tarballs.project(name), entry["integrity"], extracted[name].as_output()],
                    category = "npm_extract",
                    # Unique per tarball (an alias and a nested package can
                    # share a name and version) and independent of where
                    # the lockfile installs it.
                    identifier = "{}@{}/{}".format(
                        entry.get("name") or install_path.split("node_modules/")[-1],
                        entry.get("version", "?"),
                        name.removesuffix(".tgz"),
                    ),
                )
            pkg_dir = extracted[name]
        package = {
            "bin": entry.get("bin", {}),
            "dev": bool(entry.get("dev")),
            # None for bundled dependencies: the parent's directory has them.
            "dir": pkg_dir,
            # Relative to the node_modules output itself.
            "path": install_path.removeprefix("node_modules/"),
        }
        if entry.get("binDir"):
            # Only in converted bun.lock files: link every file in this directory.
            package["bin_dir"] = entry["binDir"]
        packages.append(package)

    for variant, output, pkgs in [
        ("dev", out, packages),
        ("prod", prod_out, [p for p in packages if not p["dev"]]),
    ]:
        manifest = actions.write_json(variant + "_manifest.json", {"packages": pkgs}, with_inputs = True)
        actions.run(
            [node.node, tool, "assemble", manifest, output],
            category = "npm_node_modules",
            identifier = variant,
        )
    return []

_node_modules = dynamic_actions(
    impl = _node_modules_impl,
    attrs = {
        "extract_tool": dynattrs.value(Artifact),
        "lockfile": dynattrs.artifact_value(),
        "node": dynattrs.value(NodeToolchainInfo),
        "out": dynattrs.output(),
        "prod_out": dynattrs.output(),
        "tarballs": dynattrs.value(Artifact),
        "tool": dynattrs.value(Artifact),
    },
)

# ---------------------------------------------------------------------------
# npm_build

def npm_build_impl(ctx: AnalysisContext) -> list[Provider]:
    node = ctx.attrs._node_toolchain[NodeToolchainInfo]
    tool = ctx.attrs._npm_tool[DefaultInfo].default_outputs[0]
    node_modules = ctx.attrs.node_modules[NpmNodeModulesInfo].node_modules

    srcs = ctx.actions.write_json(
        "srcs.json",
        {src.short_path: src for src in ctx.attrs.srcs},
        with_inputs = True,
    )
    out = ctx.actions.declare_output(ctx.attrs.out, dir = True)
    # `checks` outputs are hidden inputs: only there so the checks must pass first.
    checks = [dep[DefaultInfo].default_outputs for dep in ctx.attrs.checks]
    ctx.actions.run(
        cmd_args(
            node.node,
            tool,
            "run",
            "--node",
            node.node,
            "--npm",
            node.npm,
            "--srcs",
            srcs,
            "--node-modules",
            node_modules,
            "--script",
            ctx.attrs.script,
            "--out-dir",
            ctx.attrs.out,
            "--output",
            out.as_output(),
            ["--env", ctx.actions.write_json("env.json", ctx.attrs.env)] if ctx.attrs.env else [],
            hidden = checks,
        ),
        category = "npm_run",
        identifier = ctx.attrs.script,
    )
    return [DefaultInfo(default_output = out)]
