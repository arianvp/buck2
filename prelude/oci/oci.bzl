# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Reproducible OCI (container) images from build outputs.
#
# - `oci_pull` is the only network step: it downloads a base image pinned by
#   digest (for a multi-platform image, the target platform's manifest) and
#   checks every blob against its digest. It runs locally and its result is
#   uploaded to the remote action cache. Images built on it check the
#   manifest and config against the pinned digest, and layer blobs are
#   checked whenever their bytes are read ([tarball], oci_push).
# - `oci_image` builds an image on top of a base (or from scratch):
#   - one action per layer writes a reproducible tar (gzipped) of build
#     outputs. Its outputs are named after the layer's contents, not its
#     position, so adding, removing or reordering layers only rebuilds the
#     layers that changed;
#   - a small action writes the config and the manifest from the layers'
#     descriptors and the base's manifest and config, without reading any
#     layer: `[digest]`;
#   - the image layout is assembled with copies (no data moves: on remote
#     execution, buck2 only records digests), so changing the config doesn't
#     copy the image;
#   - `[tarball]` packs the layout, with Docker's manifest.json too, for
#     `docker load`.
# - `oci_push` is run with `buck2 run`: it pushes the image's blobs (those
#   the registry doesn't have yet) and manifest to a registry, with the
#   Docker config's credentials.
#
# Images are Linux images: oci_image and oci_pull switch the target platform
# to Linux (see transition.bzl), and the image's architecture is the target
# platform's CPU.
#
# The tools are standalone Python scripts referenced from the source tree,
# so they're the same action input in every configuration, and they run
# with `-I -B -X utf8` so the environment and the locale can't change outputs.

load("@prelude//python_bootstrap:python_bootstrap.bzl", "PythonBootstrapToolchainInfo")
load(":providers.bzl", "OciImageInfo")
load(":reference.bzl", "normalize_repository", "normalize_tagged")

_OCI_LAYOUT = '{"imageLayoutVersion":"1.0.0"}'
_DIGEST = regex("^sha256:[0-9a-f]{64}$")

def _python(ctx: AnalysisContext) -> cmd_args:
    return cmd_args(
        ctx.attrs._python_toolchain[PythonBootstrapToolchainInfo].interpreter,
        # Isolated (no PYTHON* variables, user site-packages or script
        # directory on sys.path), no .pyc files, and UTF-8 whatever the locale.
        "-I",
        "-B",
        "-X",
        "utf8",
    )

def _tool(dep: Dependency) -> Artifact:
    return dep[DefaultInfo].default_outputs[0]

def _arch(ctx: AnalysisContext) -> str:
    if not ctx.attrs.arch:
        fail("{}: no image architecture for the target platform's CPU; set `arch`".format(ctx.label.raw_target()))
    return ctx.attrs.arch

def _blob_path(digest: str) -> str:
    return "blobs/sha256/" + digest.removeprefix("sha256:")

def oci_pull_impl(ctx: AnalysisContext) -> list[Provider]:
    if not _DIGEST.match(ctx.attrs.digest):
        fail("oci_pull: `digest` must be a sha256 digest (sha256:<64 lowercase hex digits>), got `{}`".format(ctx.attrs.digest))
    image = normalize_repository("oci_pull", ctx.attrs.image)
    layout = ctx.actions.declare_output("layout", dir = True)
    manifest = ctx.actions.declare_output("manifest.json")
    config = ctx.actions.declare_output("config.json")
    pinned = ctx.actions.declare_output("pinned.json")
    ctx.actions.run(
        cmd_args(
            _python(ctx),
            _tool(ctx.attrs._oci_pull),
            "--image",
            image,
            "--digest",
            ctx.attrs.digest,
            "--os",
            "linux",
            "--arch",
            _arch(ctx),
            ["--variant", ctx.attrs.variant] if ctx.attrs.variant else [],
            ["--insecure"] if ctx.attrs.insecure else [],
            "--out",
            layout.as_output(),
            "--out-manifest",
            manifest.as_output(),
            "--out-config",
            config.as_output(),
            "--out-pinned",
            pinned.as_output(),
        ),
        category = "oci_pull",
        # Needs network access. The digest determines the result, so it is
        # shared through the remote cache.
        local_only = True,
        allow_cache_upload = True,
    )
    return [
        DefaultInfo(default_output = layout),
        OciImageInfo(
            layout = layout,
            manifest = manifest,
            config = config,
            pin = struct(digest = ctx.attrs.digest, blob = pinned),
        ),
    ]

def _layer_key(layer: dict[str, Artifact]) -> str:
    # Layers are named after their contents (destinations and the artifacts'
    # identities), not their position: output paths are part of an action's
    # key, so adding or moving another layer must not change them.
    return sha256(repr([(dest, str(src)) for dest, src in sorted(layer.items())]))[:16]

def _layer(ctx: AnalysisContext, python: cmd_args, key: str, layer: dict[str, Artifact]) -> (Artifact, Artifact):
    """Declares the action writing one layer: (blob, descriptor)."""
    if not layer:
        fail("oci_image: a layer is empty")
    for dest in layer:
        if not dest.startswith("/"):
            fail("oci_image: layer paths must be absolute, got `{}`".format(dest))
    entries = [{"dest": dest, "src": src} for dest, src in sorted(layer.items())]
    entries_file = ctx.actions.write_json("layers/{}/entries.json".format(key), entries, with_inputs = True)
    blob = ctx.actions.declare_output("layers/{}/layer.tar{}".format(key, ".gz" if ctx.attrs.compress else ""))
    desc = ctx.actions.declare_output("layers/{}/descriptor.json".format(key))
    ctx.actions.run(
        cmd_args(
            python,
            _tool(ctx.attrs._oci_layer),
            "--entries",
            entries_file,
            ["--compress"] if ctx.attrs.compress else [],
            "--out-blob",
            blob.as_output(),
            "--out-desc",
            desc.as_output(),
        ),
        category = "oci_layer",
        identifier = key,
    )
    return blob, desc

def oci_image_impl(ctx: AnalysisContext) -> list[Provider]:
    python = _python(ctx)
    tags = [normalize_tagged("oci_image", tag) for tag in ctx.attrs.tags]
    base = ctx.attrs.base[OciImageInfo] if ctx.attrs.base else None

    blobs = []
    descs = []
    declared = {}
    for layer in ctx.attrs.layers:
        key = _layer_key(layer)
        if key not in declared:
            declared[key] = _layer(ctx, python, key, layer)
        blob, desc = declared[key]
        blobs.append(blob)
        descs.append(desc)

    settings = ctx.actions.write_json("settings.json", {
        "Cmd": ctx.attrs.cmd,
        "Entrypoint": ctx.attrs.entrypoint,
        "Env": ctx.attrs.env,
        "ExposedPorts": ctx.attrs.exposed_ports,
        "Labels": ctx.attrs.image_labels,
        "User": ctx.attrs.user,
        "WorkingDir": ctx.attrs.workdir,
        "architecture": _arch(ctx),
        "os": "linux",
        "tags": [{"name": t.name, "tag": t.tag} for t in tags],
    })
    manifest = ctx.actions.declare_output("manifest.json")
    config = ctx.actions.declare_output("config.json")
    index = ctx.actions.declare_output("index.json")
    digest = ctx.actions.declare_output("digest")
    base_args = []
    if base:
        base_args = ["--base-manifest", base.manifest, "--base-config", base.config]
        if base.pin:
            base_args += ["--base-pin", base.pin.digest, base.pin.blob]
    ctx.actions.run(
        cmd_args(
            python,
            _tool(ctx.attrs._oci_image),
            "manifest",
            "--settings",
            settings,
            base_args,
            [["--layer", desc] for desc in descs],
            "--out-manifest",
            manifest.as_output(),
            "--out-config",
            config.as_output(),
            "--out-index",
            index.as_output(),
            "--out-digest",
            digest.as_output(),
        ),
        category = "oci_manifest",
    )

    # The layout's blobs are copies of the base's and the layers' (buck2
    # copies by digest, without reading them).
    layout = ctx.actions.declare_output("layout", dir = True)
    oci_layout = ctx.actions.write("oci-layout", _OCI_LAYOUT)

    def assemble(ctx: AnalysisContext, artifacts, outputs):
        manifest_json = artifacts[manifest].read_json()
        layers = [layer["digest"] for layer in manifest_json["layers"]]
        n_base = len(layers) - len(blobs)
        srcs = {
            "index.json": index,
            "oci-layout": oci_layout,
            _blob_path(artifacts[digest].read_string().strip()): manifest,
            _blob_path(manifest_json["config"]["digest"]): config,
        }
        for layer_digest in layers[:n_base]:
            srcs.setdefault(_blob_path(layer_digest), base.layout.project(_blob_path(layer_digest)))
        for layer_digest, blob in zip(layers[n_base:], blobs):
            srcs.setdefault(_blob_path(layer_digest), blob)
        ctx.actions.copied_dir(outputs[layout], srcs)

    ctx.actions.dynamic_output(
        dynamic = [manifest, digest],
        inputs = [],
        outputs = [layout.as_output()],
        f = assemble,
    )

    tarball = ctx.actions.declare_output(ctx.label.name + ".tar")
    ctx.actions.run(
        cmd_args(python, _tool(ctx.attrs._oci_image), "tarball", "--layout", layout, "--out", tarball.as_output()),
        category = "oci_tarball",
    )

    return [
        DefaultInfo(
            default_output = layout,
            sub_targets = {
                "digest": [DefaultInfo(default_output = digest)],
                "tarball": [DefaultInfo(default_output = tarball)],
            },
        ),
        OciImageInfo(layout = layout, manifest = manifest, config = config),
    ]

def oci_push_impl(ctx: AnalysisContext) -> list[Provider]:
    # Pushing is a side effect that needs credentials, so it is not a build
    # action: `buck2 run` builds the image and runs this locally.
    layout = ctx.attrs.image[OciImageInfo].layout
    for tag in ctx.attrs.tags:
        normalize_tagged("oci_push", "x:" + tag)
    repository = normalize_repository("oci_push", ctx.attrs.repository)
    args = cmd_args(
        _python(ctx),
        _tool(ctx.attrs._oci_push),
        "--layout",
        layout,
        "--repository",
        repository,
        [["--tag", tag] for tag in ctx.attrs.tags],
        # Only this registry: `-- --repository` elsewhere uses HTTPS.
        ["--insecure-registry", repository.partition("/")[0]] if ctx.attrs.insecure else [],
    )
    return [
        DefaultInfo(default_output = layout),
        RunInfo(args = args),
    ]

def _default_arch():
    # The OCI (GOARCH) name of the target platform's CPU; empty (an error
    # unless `arch` is set) for others.
    return select({
        "prelude//cpu:arm32": "arm",
        "prelude//cpu:arm64": "arm64",
        "prelude//cpu:riscv64": "riscv64",
        "prelude//cpu:x86_32": "386",
        "prelude//cpu:x86_64": "amd64",
        "DEFAULT": "",
    })

_common_attrs = {
    "_python_toolchain": attrs.toolchain_dep(default = "toolchains//:python_bootstrap", providers = [PythonBootstrapToolchainInfo]),
}

_arch_attr = attrs.string(
    default = _default_arch(),
    doc = "The image's architecture (`amd64`, `arm64`, ...); by default the target platform's CPU.",
)

extra_attributes = {
    "oci_image": _common_attrs | {
        "arch": _arch_attr,
        "_oci_image": attrs.default_only(attrs.dep(default = "prelude//oci/tools:oci_image.py")),
        "_oci_layer": attrs.default_only(attrs.dep(default = "prelude//oci/tools:oci_layer.py")),
    },
    "oci_pull": _common_attrs | {
        "arch": _arch_attr,
        "_oci_pull": attrs.default_only(attrs.dep(default = "prelude//oci/tools:oci_pull.py")),
    },
    "oci_push": _common_attrs | {
        "_oci_push": attrs.default_only(attrs.dep(default = "prelude//oci/tools:oci_push.py")),
    },
}

implemented_rules = {
    "oci_image": oci_image_impl,
    "oci_pull": oci_pull_impl,
    "oci_push": oci_push_impl,
}
