# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@prelude//oci:providers.bzl", "OciImageInfo")
load("@prelude//oci:transition.bzl", "oci_linux_transition")
load(":common.bzl", "buck", "prelude_rule")

oci_pull = prelude_rule(
    name = "oci_pull",
    docs = """
        An `oci_pull()` rule downloads a container image pinned by digest,
         for use as the base of an `oci_image()`. For a multi-platform image
         (an index), the manifest for linux/`arch`/`variant` is used. Every
         download is checked against its sha256 digest, so the output only
         depends on `digest` and the platform: it runs locally (it needs the
         network), and its result is uploaded to the remote action cache for
         other machines. Images built on it check its manifest and config
         against `digest` again, and the layers are checked whenever they
         are read ([tarball], `oci_push`), so a wrong cache entry can't
         change an image unnoticed.

        Like `oci_image()`, it is configured for Linux on the target
         platform's CPU. Declare each base image once (e.g. in a
         `//third_party/oci` package) and depend on that: the download is
         cached per target.

        Public images only: registries that hand out anonymous tokens
         (Docker Hub, ghcr.io, gcr.io, ...) work, credentials are not
         supported. Python 3 (the `toolchains//:python_bootstrap` toolchain)
         runs the download.
    """,
    examples = """
        ```
        oci_pull(
            name = "distroless_static",
            image = "gcr.io/distroless/static-debian12",
            # `crane digest gcr.io/distroless/static-debian12:latest`
            digest = "sha256:d75cdd72874d4790092fcb1b058493ecf6bb5bf2b2b897045b00ff01d91843f2",
        )
        ```
    """,
    further = None,
    cfg = oci_linux_transition,
    attrs = (
        # @unsorted-dict-items
        {
            "image": attrs.string(doc = "Image repository, e.g. `gcr.io/distroless/static-debian12` or `node` (Docker Hub)."),
            "digest": attrs.string(doc = "sha256 digest of the image manifest or index."),
            "variant": attrs.string(default = "", doc = "CPU variant to use from a multi-platform image (e.g. `v7` for `arm`); any if empty."),
            "insecure": attrs.bool(default = False, doc = "Use plain HTTP (e.g. a local test registry)."),
        } |
        buck.licenses_arg() |
        buck.labels_arg() |
        buck.contacts_arg()
    ),
)

oci_image = prelude_rule(
    name = "oci_image",
    docs = """
        An `oci_image()` rule builds a Linux container image from build
         outputs, on top of an `oci_pull()` (or another `oci_image()`) or
         from scratch. The output is an OCI image layout directory;
         `[tarball]` is it packed into a tar for `docker load` (with Docker's
         `manifest.json` too), and `[digest]` the image manifest's digest.

        The image is configured for Linux on the target platform's CPU: on
         another OS (e.g. macOS), the layers' contents are built for Linux
         (Go binaries are cross compiled, toolchain archives are the Linux
         ones). The image's architecture is the target platform's CPU
         (`arch` overrides it).

        Layers are reproducible tars: entries sorted, timestamps 0, root as
         owner, modes 0755 for directories and executables and 0644 for
         other files; the config's `created` is 1970-01-01T00:00:00Z. Each
         layer is built by its own action named after its contents, so a
         change only rebuilds the layers it touches, wherever they are in
         the list. The uncompressed layers (the
         config's diff IDs) only depend on the inputs. gzip output also
         depends on the zlib implementation: layers are only compressed with
         a zlib that matches the reference zlib (1.2.x and 1.3.x do; zlib-ng,
         the default on Fedora 40+, does not), and the build fails otherwise;
         build there on remote execution, or set `compress = False`.

        Only the sources' contents go in a layer, not the destinations'
         parent directories, so the base image's directories (and symlinks
         such as `/bin -> usr/bin`) keep their owner, mode and type; missing
         parents are created by the runtime (root, 0755), as with Dockerfile
         `COPY`. Symlinks inside a directory source are kept if they stay in
         it (or are absolute and point outside the build); links out of it
         (like buck2's symlinked directories' links into buck-out) are
         replaced by what they point to.
    """,
    examples = """
        ```
        oci_image(
            name = "image",
            base = ":distroless_static",
            layers = [{"/server": ":server"}],
            entrypoint = ["/server"],
            exposed_ports = ["8080"],
            user = "nonroot",
        )
        ```
    """,
    further = None,
    cfg = oci_linux_transition,
    attrs = (
        # @unsorted-dict-items
        {
            "base": attrs.option(attrs.dep(providers = [OciImageInfo]), default = None, doc = "An `oci_pull()` or `oci_image()` to build on; from scratch if not set."),
            "layers": attrs.list(
                attrs.dict(attrs.string(), attrs.source(allow_directory = True)),
                default = [],
                doc = """
                    Layers, in order, each mapping absolute paths in the image to
                    files or directories (a directory's contents go under its
                    path, `/` for the root). Names starting with `.wh.` (deletions
                    in OCI layers) are not allowed.
                """,
            ),
            "compress": attrs.bool(default = True, doc = "gzip the layers."),
            "entrypoint": attrs.option(attrs.list(attrs.string()), default = None),
            "cmd": attrs.option(attrs.list(attrs.string()), default = None),
            "env": attrs.dict(attrs.string(), attrs.string(), default = {}, doc = "Added to (or replacing) the base image's environment."),
            "workdir": attrs.option(attrs.string(), default = None),
            "user": attrs.option(attrs.string(), default = None),
            "exposed_ports": attrs.list(attrs.string(), default = [], doc = "e.g. `8080` or `53/udp`."),
            "image_labels": attrs.dict(attrs.string(), attrs.string(), default = {}, doc = "The image config's labels."),
            "tags": attrs.list(attrs.string(), default = [], doc = """
                Names for `docker load` and OCI layout tools, e.g. `myapp:1.2`
                (`docker.io/library/myapp:1.2`) or `ghcr.io/me/app` (`:latest`).
            """),
        } |
        buck.licenses_arg() |
        buck.labels_arg() |
        buck.contacts_arg()
    ),
)

oci_push = prelude_rule(
    name = "oci_push",
    docs = """
        An `oci_push()` rule pushes an `oci_image()` to a registry when run
         with `buck2 run`: blobs the registry already has are skipped, then
         the manifest is pushed by digest and under each tag. It prints
         `<repository>@<digest>`. Arguments after `--` add tags (`--tag v2`)
         or push to another repository (`--repository ...`, over HTTPS unless
         its registry host is given with `--insecure-registry HOST`).

        Pushing is not a build action (it has side effects and needs
         credentials), so it runs locally, every time. Credentials come from
         the Docker config (`$DOCKER_CONFIG/config.json` or
         `~/.docker/config.json`: `auths`, `credHelpers`, `credsStore`), and
         are only sent to the registry and, when it asks for a token, to the
         token server it names (over HTTPS).
    """,
    examples = """
        ```
        oci_push(
            name = "push",
            image = ":image",
            repository = "ghcr.io/me/server",
            tags = ["latest"],
        )
        ```

        `buck2 run //app:push -- --tag v1.2.3`
    """,
    further = None,
    attrs = (
        # @unsorted-dict-items
        {
            "image": attrs.dep(providers = [OciImageInfo], doc = "The `oci_image()` to push."),
            "repository": attrs.string(doc = "Repository to push to, e.g. `ghcr.io/me/server` (no tag)."),
            "tags": attrs.list(attrs.string(), default = [], doc = "Tags to push the image under."),
            "insecure": attrs.bool(default = False, doc = "Reach `repository`'s registry over plain HTTP (e.g. a local test registry). Only that registry: pushing elsewhere with `--repository` uses HTTPS."),
        } |
        buck.licenses_arg() |
        buck.labels_arg() |
        buck.contacts_arg()
    ),
)

oci_rules = struct(
    oci_image = oci_image,
    oci_pull = oci_pull,
    oci_push = oci_push,
)
