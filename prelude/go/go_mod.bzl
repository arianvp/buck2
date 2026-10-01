# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Lockfile-driven Go builds: build a Go module straight from its go.mod/go.sum
# without generating BUCK files for third-party packages.
#
# - `go_mod_download` is the only step that touches the network. It runs
#   `go mod download`, which verifies every module against go.sum, and outputs
#   a module cache directory, which only depends on the go.mod files, go.sum
#   and the module proxy. A separate action (`go mod verify`, which can run
#   remotely) checks that cache against go.sum before anything uses it.
# - `go_mod_binary` runs `go list -deps` offline against that cache, then (in a
#   dynamic action) declares one compile action per package. Each compile only
#   sees the package's own files (projected out of the cache or taken from
#   `srcs`) and the export data of its direct imports, so bumping one module
#   only rebuilds that module's packages and their dependents.

load("@prelude//:paths.bzl", "paths")
load("@prelude//utils:graph_utils.bzl", "post_order_traversal")
load(":package_builder.bzl", "BuildPackageGoList", "BuildPackageParams", "build_package")
load(
    ":packages.bzl",
    "GoPkg",
    "GoStdlib",
    "GoStdlibDynamicValue",
    "make_link_importcfg",
    "merge_pkgs",
)
load(":toolchain.bzl", "GoToolchainInfo", "get_toolchain_env_vars")

GoModCacheInfo = provider(
    fields = {
        # A GOMODCACHE directory populated by `go mod download`.
        "cache": provider_field(Artifact),
        # The output of `go mod verify` on `cache`: depend on it to only
        # read a verified cache.
        "verified": provider_field(Artifact),
    },
)

# Variables `go mod download` keeps from buck2's environment besides the
# declared ones: they change how `go` reaches the module proxy (proxies, CA
# certificates, credentials), not which bytes it gets, which go.sum pins.
_NETWORK_ENV = [
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NETRC",
    "NO_PROXY",
    "SSL_CERT_DIR",
    "SSL_CERT_FILE",
    "https_proxy",
    "http_proxy",
    "no_proxy",
]

def _clean_env_args(env: dict[str, typing.Any], keep: list[str] = []) -> list[str]:
    # buck2 runs local actions in its own environment, which is not part of
    # the action key and which remote actions never see: GOPRIVATE, GOFLAGS,
    # GOFIPS140, GOPATH, git configuration... from the user's shell would
    # change what `go` does. go_wrapper drops all of it.
    return ["--clean-env", "--keep-env=" + ",".join(sorted(env.keys()) + keep)]

def _module_go_env(mod_cache: typing.Any) -> dict[str, typing.Any]:
    # Go settings of the actions that work on a module cache. Callers also
    # set GOPROXY, which GOROOT/go.env would set otherwise, and everything
    # else is left unset by go_wrapper --clean-env.
    return {
        "GO111MODULE": "on",
        "GODEBUG": "",
        "GOENV": "off",
        "GOFIPS140": "off",
        "GOMODCACHE": cmd_args("%cwd%/", mod_cache, delimiter = ""),
        # go.sum is the only source of checksums: the checksum database's
        # answers (and the records go caches) depend on when it was asked.
        "GOSUMDB": "off",
        "GOTOOLCHAIN": "local",
        "GOWORK": "off",
    }

def _offline_go_env(go_toolchain: GoToolchainInfo, mod_cache: Artifact) -> dict[str, typing.Any]:
    env = _module_go_env(mod_cache)
    env.update(get_toolchain_env_vars(go_toolchain))
    env.update({
        "CGO_ENABLED": "0",
        "GOFLAGS": "-mod=readonly",
        "GOPROXY": "off",
    })
    if go_toolchain.env_go_root != None:
        # `go list` only reads GOROOT/src: keep the rest of the distribution
        # (compiled tools, tests: more than half of its bytes) out of the
        # action's inputs.
        env["GOROOT"] = cmd_args(go_toolchain.env_go_root.project("src"), parent = 1)
    return env

def go_mod_download_impl(ctx: AnalysisContext) -> list[Provider]:
    go_toolchain = ctx.attrs._go_toolchain[GoToolchainInfo]

    for proxy in ctx.attrs.goproxy.replace("|", ",").split(","):
        if proxy.strip() == "direct":
            fail("go_mod_download: `goproxy` must not contain `direct`: fetching modules from version control runs the host's git with the user's configuration and leaves VCS state in the module cache. Use a module proxy.")

    # Of the modules that `replace` directives point to inside the module
    # root, `go mod download` only reads the go.mod files.
    root = paths.dirname(ctx.attrs.go_mod.short_path)
    module_files = {"go.mod": ctx.attrs.go_mod, "go.sum": ctx.attrs.go_sum}
    for go_mod in ctx.attrs.nested_go_mods:
        rel = paths.relativize(go_mod.short_path, root) if root else go_mod.short_path
        if rel == "go.mod" or paths.basename(rel) != "go.mod":
            fail("go_mod_download: `nested_go_mods` must be go.mod files in subdirectories of the module root, got `{}`".format(go_mod.short_path))
        module_files[rel] = go_mod
    module_dir = ctx.actions.symlinked_dir("__module__", module_files)

    # Not content-based: packages are projected out of this directory, and a
    # path that hashes the whole cache would change every compile command line
    # (and so rebuild every package) whenever any single module changes.
    cache = ctx.actions.declare_output("modcache", dir = True, has_content_based_path = False)

    env = _module_go_env(cache.as_output())
    env.update({
        "GOAUTH": "netrc",
        # Keep the cache writable so buck2 can clean up / overwrite outputs.
        "GOFLAGS": "-mod=readonly -modcacherw",
        "GOPROXY": ctx.attrs.goproxy,
    })
    if go_toolchain.env_go_root != None:
        env["GOROOT"] = go_toolchain.env_go_root

    ctx.actions.run(
        [
            go_toolchain.go_wrapper,
            ["--go", go_toolchain.go],
            _clean_env_args(env, _NETWORK_ENV),
            # Without GOSUMDB, go accepts what go.sum does not list: fail
            # unless go.sum pins every module and go.mod file downloaded.
            ["--check-mod-cache-sums", ctx.attrs.go_sum],
            # Only the extracted modules (and their .mod/.ziphash files) are
            # needed to load and compile packages: dropping the zips keeps
            # about a fifth of the cache's bytes out of the upload to the
            # action cache and out of the inputs of `go list`.
            "--prune-mod-cache",
            "--",
            ["-C", module_dir],
            "mod",
            "download",
        ],
        env = env,
        category = "go_mod_download",
        # Needs network access. The environment is cleared, so the output only
        # depends on the declared inputs (go.mod files, go.sum, GOPROXY) and
        # can be shared.
        local_only = True,
        allow_cache_upload = True,
    )

    # The download's result may come from the action cache, uploaded by
    # whichever machine ran it, and go list only compares the .ziphash files
    # with go.sum, never the extracted modules. Hash them (`go mod verify`)
    # in an action of its own: it is keyed on the cache, the go.mod files and
    # go.sum only, so it runs once per module cache, wherever that came from,
    # and go list waits for it.
    verified = ctx.actions.declare_output("mod_verify.txt")
    verify_env = _module_go_env(cache)
    verify_env.update({
        "GOFLAGS": "-mod=readonly",
        "GOPROXY": "off",
    })
    if go_toolchain.env_go_root != None:
        # `go mod verify` needs nothing from GOROOT but the go command.
        verify_env["GOROOT"] = cmd_args(go_toolchain.env_go_root.project("bin"), parent = 1)
    ctx.actions.run(
        [
            go_toolchain.go_wrapper,
            ["--go", go_toolchain.go],
            _clean_env_args(verify_env),
            # `go mod verify` checks the extracted modules against their
            # .ziphash files; this checks those against go.sum.
            ["--check-mod-cache-sums", ctx.attrs.go_sum],
            ["--output", verified.as_output()],
            "--",
            ["-C", module_dir],
            "mod",
            "verify",
        ],
        env = verify_env,
        category = "go_mod_verify",
        allow_cache_upload = go_toolchain.allow_cache_upload,
    )

    return [
        DefaultInfo(default_output = cache, other_outputs = [verified]),
        GoModCacheInfo(cache = cache, verified = verified),
    ]

_GO_LIST_FIELDS = ",".join([
    "ImportPath",
    "Name",
    "Dir",
    "Standard",
    "DepOnly",
    "Module",
    "GoFiles",
    "CgoFiles",
    "HFiles",
    "CFiles",
    "CXXFiles",
    "SFiles",
    "SysoFiles",
    "Imports",
    "ImportMap",
    "EmbedPatterns",
    "EmbedFiles",
    "Error",
    "DepsErrors",
])

def go_mod_binary_impl(ctx: AnalysisContext) -> list[Provider]:
    go_toolchain = ctx.attrs._go_toolchain[GoToolchainInfo]
    go_stdlib = ctx.attrs._go_stdlib[GoStdlib]
    mod_cache_info = ctx.attrs.mod_cache[GoModCacheInfo]
    mod_cache = mod_cache_info.cache

    # Map module-root-relative paths (the target lives next to go.mod) to
    # first-party sources.
    srcs = {src.short_path: src for src in ctx.attrs.srcs}
    if "go.mod" not in srcs:
        fail("go_mod_binary: `srcs` must include go.mod (the target must live next to go.mod)")

    # Copied rather than symlinked: `go list` rejects symlinks for //go:embed.
    module_dir = ctx.actions.copied_dir("__module__", srcs)

    tags = go_toolchain.build_tags + ctx.attrs._build_tags
    go_list_out = ctx.actions.declare_output("go_list.json")
    env = _offline_go_env(go_toolchain, mod_cache)
    ctx.actions.run(
        [
            go_toolchain.go_wrapper,
            ["--go", go_toolchain.go],
            _clean_env_args(env),
            # Fail (before compiling anything) unless the module cache passed
            # `go mod verify`.
            cmd_args(hidden = mod_cache_info.verified),
            ["--output", go_list_out.as_output()],
            "--convert-json-stream",
            # GOROOT, GOMODCACHE and the module directory are absolute paths
            # under the action's working directory, which differs between
            # executions on RE: make them relative so the output does not.
            "--trim-cwd",
            "--",
            ["-C", module_dir],
            "list",
            "-deps",
            "-e",
            "-json=" + _GO_LIST_FIELDS,
            ["-tags", ",".join(tags)] if tags else [],
            ctx.attrs.package,
        ],
        env = env,
        category = "go_list_module",
        allow_cache_upload = go_toolchain.allow_cache_upload,
    )

    bin = ctx.actions.declare_output(ctx.label.name)
    ctx.actions.dynamic_output_new(
        _build_module(
            go_list_out = go_list_out,
            go_stdlib_value = go_stdlib.dynamic_value,
            go_toolchain = go_toolchain,
            target_label = ctx.label,
            srcs = srcs,
            mod_cache = mod_cache,
            compiler_flags = ctx.attrs.compiler_flags,
            linker_flags = ctx.attrs.linker_flags,
            out = bin.as_output(),
        ),
    )

    return [
        DefaultInfo(
            default_output = bin,
            sub_targets = {"go_list": [DefaultInfo(default_output = go_list_out)]},
        ),
        RunInfo(args = cmd_args(bin)),
    ]

def _escape_module_path(s: str) -> str:
    # Module cache paths encode upper-case letters as "!" + lower-case,
    # see golang.org/x/mod/module.EscapePath.
    out = []
    for i in range(len(s)):
        c = s[i]
        if c.isupper():
            out.append("!" + c.lower())
        else:
            out.append(c)
    return "".join(out)

def _files_resolver(pkg: dict, main_dir: str, srcs: dict[str, Artifact], mod_cache: Artifact):
    """Returns (package_root, fn(filename) -> Artifact) for a non-std package."""
    pkg_dir = pkg["Dir"]
    import_path = pkg["ImportPath"]

    if pkg_dir == main_dir or pkg_dir.startswith(main_dir + "/"):
        # First-party package (main module, or a replace directive into it).
        rel_dir = pkg_dir.removeprefix(main_dir).lstrip("/")

        def first_party(f: str) -> Artifact:
            rel = paths.join(rel_dir, f) if rel_dir else f
            if rel not in srcs:
                fail("go_mod_binary: package `{}` needs `{}`, which is missing from `srcs`".format(import_path, rel))
            return srcs[rel]

        return (paths.join("__module__", rel_dir), first_party)

    module = pkg.get("Module")
    if module == None:
        fail("go_mod_binary: package `{}` is outside the main module but has no module info".format(import_path))
    location = module.get("Replace") or module
    if not location.get("Version"):
        fail("go_mod_binary: package `{}` comes from a local replace directive outside the module root, which is not supported".format(import_path))

    cache_rel = "{}@{}{}".format(
        _escape_module_path(location["Path"]),
        _escape_module_path(location["Version"]),
        pkg_dir.removeprefix(location["Dir"]),
    )

    def third_party(f: str) -> Artifact:
        return mod_cache.project(paths.join(cache_rel, f))

    return (paths.join("__modcache__", cache_rel), third_party)

def _build_module_impl(
    actions: AnalysisActions,
    go_list_out: ArtifactValue,
    go_stdlib_value: ResolvedDynamicValue,
    go_toolchain: GoToolchainInfo,
    target_label: Label,
    srcs: dict[str, Artifact],
    mod_cache: Artifact,
    compiler_flags: list[str],
    linker_flags: list[typing.Any],
    out: OutputArtifact,
) -> list[Provider]:
    stdlib_pkgs = go_stdlib_value.providers[GoStdlibDynamicValue].pkgs
    listed = go_list_out.read_json()

    errors = []
    main_dir = None
    for pkg in listed:
        if pkg.get("Error"):
            errors.append("{}: {}".format(pkg["ImportPath"], pkg["Error"]["Err"]))
        if pkg.get("Module") and pkg["Module"].get("Main"):
            main_dir = pkg["Module"]["Dir"]
    if errors:
        fail("go_mod_binary: `go list` reported errors:\n  " + "\n  ".join(errors))

    # Collect non-std packages and their in-module import graph.
    pkgs_by_path = {}
    roots = []
    for pkg in listed:
        if pkg.get("Standard"):
            continue
        pkgs_by_path[pkg["ImportPath"]] = pkg
        if not pkg.get("DepOnly"):
            roots.append(pkg)

    graph = {
        path: [imp for imp in pkg.get("Imports", []) if imp in pkgs_by_path]
        for path, pkg in pkgs_by_path.items()
    }

    main_pkgs = [p for p in roots if p.get("Name") == "main"]
    if len(main_pkgs) != 1:
        fail("go_mod_binary: `package` must match exactly one main package, got: {}".format([p["ImportPath"] for p in roots]))
    main_import_path = main_pkgs[0]["ImportPath"]

    built = {}
    for import_path in post_order_traversal(graph):
        pkg = pkgs_by_path[import_path]
        package_root, resolve = _files_resolver(pkg, main_dir, srcs, mod_cache)

        if pkg.get("CgoFiles") or pkg.get("CFiles") or pkg.get("CXXFiles"):
            fail("go_mod_binary: cgo is not supported (package `{}`)".format(import_path))

        is_main = import_path == main_import_path
        built[import_path] = _declare_package(
            actions = actions,
            target_label = target_label,
            go_toolchain = go_toolchain,
            go_list = BuildPackageGoList(
                pkg_name = pkg.get("Name", ""),
                go_files = [resolve(f) for f in pkg.get("GoFiles", [])],
                cgo_files = [],
                s_files = [resolve(f) for f in pkg.get("SFiles", [])],
                h_files = [resolve(f) for f in pkg.get("HFiles", [])],
                c_cxx_files = [],
                syso_files = [resolve(f) for f in pkg.get("SysoFiles", [])],
                imports = set(pkg.get("Imports", [])),
                embed_patterns = pkg.get("EmbedPatterns", []),
                cgo_cflags = [],
                cgo_cppflags = [],
            ),
            params = BuildPackageParams(
                main = is_main,
                standard = False,
                pkg_import_path = import_path,
                package_root = package_root,
                embed_srcs = {paths.join(package_root, f): resolve(f) for f in pkg.get("EmbedFiles", [])},
                compiler_flags = compiler_flags,
                assembler_flags = [],
                coverage_enabled = False,
                coverage_mode = None,
                deps = merge_pkgs([stdlib_pkgs, built]),
                import_map = pkg.get("ImportMap") or None,
            ),
        )

    _link(
        actions = actions,
        go_toolchain = go_toolchain,
        main_pkg = built[main_import_path],
        deps = merge_pkgs([stdlib_pkgs, built]),
        linker_flags = linker_flags,
        identifier = str(target_label.name),
        out = out,
    )
    return []

_build_module = dynamic_actions(
    impl = _build_module_impl,
    # @unsorted-dict-items
    attrs = {
        "go_list_out": dynattrs.artifact_value(),
        "go_stdlib_value": dynattrs.dynamic_value(),  # GoStdlibDynamicValue
        "go_toolchain": dynattrs.value(GoToolchainInfo),
        "target_label": dynattrs.value(Label),
        "srcs": dynattrs.value(dict[str, Artifact]),
        "mod_cache": dynattrs.value(Artifact),
        "compiler_flags": dynattrs.value(list[str]),
        "linker_flags": dynattrs.value(list[typing.Any]),
        "out": dynattrs.output(),
    },
)

def _declare_package(
    actions: AnalysisActions,
    target_label: Label,
    go_toolchain: GoToolchainInfo,
    go_list: BuildPackageGoList,
    params: BuildPackageParams) -> GoPkg:
    # Each package is built in its own nested dynamic action so that the
    # intermediate artifacts declared by `build_package` get their own namespace.
    prefix = "pkgs/" + params.pkg_import_path
    out_a = actions.declare_output(prefix + "/non-shared.a", has_content_based_path = True)
    out_x = actions.declare_output(prefix + "/non-shared.x", has_content_based_path = True)
    out_a_shared = actions.declare_output(prefix + "/shared.a", has_content_based_path = True)
    out_x_shared = actions.declare_output(prefix + "/shared.x", has_content_based_path = True)

    actions.dynamic_output_new(
        _build_package(
            target_label = target_label,
            go_toolchain = go_toolchain,
            go_list = go_list,
            params = params,
            out_a = out_a.as_output(),
            out_x = out_x.as_output(),
            out_a_shared = out_a_shared.as_output(),
            out_x_shared = out_x_shared.as_output(),
        ),
    )

    return GoPkg(
        archive_file = out_a,
        archive_file_shared = out_a_shared,
        export_file = out_x,
        export_file_shared = out_x_shared,
        coverage_instrumented = False,
    )

def _build_package_impl(
    actions: AnalysisActions,
    target_label: Label,
    go_toolchain: GoToolchainInfo,
    go_list: BuildPackageGoList,
    params: BuildPackageParams,
    out_a: OutputArtifact,
    out_x: OutputArtifact,
    out_a_shared: OutputArtifact,
    out_x_shared: OutputArtifact) -> list[Provider]:
    result = build_package(
        actions = actions,
        target_label = target_label,
        go_toolchain = go_toolchain,
        cgo_build_context = None,
        go_list = go_list,
        params = params,
    )
    actions.copy_file(out_a, result.a_file)
    actions.copy_file(out_x, result.x_file)
    actions.copy_file(out_a_shared, result.a_file_shared)
    actions.copy_file(out_x_shared, result.x_file_shared)
    return []

_build_package = dynamic_actions(
    impl = _build_package_impl,
    # @unsorted-dict-items
    attrs = {
        "target_label": dynattrs.value(Label),
        "go_toolchain": dynattrs.value(GoToolchainInfo),
        "go_list": dynattrs.value(BuildPackageGoList),
        "params": dynattrs.value(BuildPackageParams),
        "out_a": dynattrs.output(),
        "out_x": dynattrs.output(),
        "out_a_shared": dynattrs.output(),
        "out_x_shared": dynattrs.output(),
    },
)

def _link(
    actions: AnalysisActions,
    go_toolchain: GoToolchainInfo,
    main_pkg: GoPkg,
    deps: dict[str, GoPkg],
    linker_flags: list[typing.Any],
    identifier: str,
    out: OutputArtifact):
    importcfg = make_link_importcfg(actions, deps, shared = False)
    actions.run(
        [
            go_toolchain.go_wrapper,
            ["--go", go_toolchain.linker],
            "--",
            go_toolchain.linker_flags,
            linker_flags,
            "-buildmode=exe",
            "-buildid=",
            ["-importcfg", importcfg],
            ["-o", out],
            main_pkg.archive_file,
        ],
        env = get_toolchain_env_vars(go_toolchain),
        category = "go_link",
        identifier = identifier,
        allow_cache_upload = go_toolchain.allow_cache_upload,
    )
