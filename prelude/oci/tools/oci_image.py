# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

"""Write OCI image manifests and image tarballs.

    oci_image.py manifest --settings SETTINGS.json
            [--base-manifest FILE --base-config FILE [--base-pin DIGEST FILE]]
            [--layer DESCRIPTOR]... --out-manifest FILE --out-config FILE
            --out-index FILE --out-digest FILE
        Writes the image's config and manifest: the base image's layers (if
        any) followed by the given layers (oci_layer.py descriptors), with
        SETTINGS.json's settings applied on top of the base image's config,
        and an OCI layout index.json for it (one entry per tag). Only reads
        metadata, not layers. With --base-pin, the base manifest must be the
        pinned manifest, or listed in the pinned index.

    oci_image.py tarball --layout DIR --out FILE
        Packs an OCI image layout holding one image into a tar for
        `docker load` (with Docker's manifest.json too, for Docker's classic
        image store), checking every blob against its digest.

Outputs are reproducible: JSON is canonical (sorted keys, no spaces), the
config's `created` and the history entries added for new layers are
1970-01-01T00:00:00Z (a base image's entries are kept as they are), and the
tarball's entries are sorted, with timestamps 0 and root as owner.
"""

import argparse
import hashlib
import json
import os
import re
import sys
import tarfile

LAYER_TAR = "application/vnd.oci.image.layer.v1.tar"
LAYER_TAR_GZIP = "application/vnd.oci.image.layer.v1.tar+gzip"
LAYER_TAR_ZSTD = "application/vnd.oci.image.layer.v1.tar+zstd"
MANIFEST = "application/vnd.oci.image.manifest.v1+json"
INDEX = "application/vnd.oci.image.index.v1+json"
CONFIG = "application/vnd.oci.image.config.v1+json"
DOCKER_MANIFEST = "application/vnd.docker.distribution.manifest.v2+json"
DOCKER_INDEX = "application/vnd.docker.distribution.manifest.list.v2+json"
EPOCH = "1970-01-01T00:00:00Z"

# Layer media types of a base image, as the OCI types with the same bytes.
LAYER_TYPES = {
    LAYER_TAR: LAYER_TAR,
    LAYER_TAR_GZIP: LAYER_TAR_GZIP,
    LAYER_TAR_ZSTD: LAYER_TAR_ZSTD,
    "application/vnd.docker.image.rootfs.diff.tar": LAYER_TAR,
    "application/vnd.docker.image.rootfs.diff.tar.gzip": LAYER_TAR_GZIP,
}

DIGEST = re.compile(r"sha256:[0-9a-f]{64}")


def die(msg):
    sys.stderr.write("oci_image: {}\n".format(msg))
    sys.exit(1)


def canonical_json(obj):
    return json.dumps(obj, sort_keys = True, separators = (",", ":")).encode()


def sha256(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def read(path):
    with open(path, "rb") as f:
        return f.read()


def read_json(path):
    with open(path, encoding = "utf-8") as f:
        return json.load(f)


def write(path, data):
    with open(path, "wb") as f:
        f.write(data)


def check_digest(digest, what):
    if not isinstance(digest, str) or not DIGEST.fullmatch(digest):
        die("{} has an unsupported digest {!r} (only sha256 is supported)".format(what, digest))
    return digest


def media_type(doc):
    """A manifest's or index's media type: its `mediaType`, which is optional
    in OCI, or else what its fields say it is."""
    if doc.get("mediaType"):
        return doc["mediaType"]
    return INDEX if "manifests" in doc else MANIFEST


# ---------------------------------------------------------------------------
# manifest


def check_base(args, settings):
    """Returns the base image's (layer descriptors, config), checked against
    each other and against the pinned digest."""
    manifest_bytes = read(args.base_manifest)
    manifest_digest = sha256(manifest_bytes)
    manifest = json.loads(manifest_bytes)
    if media_type(manifest) not in (MANIFEST, DOCKER_MANIFEST):
        die("the base manifest is a {}, not an image manifest".format(media_type(manifest)))

    if args.base_pin:
        pin_digest, pin_path = args.base_pin
        pin_bytes = read(pin_path)
        if sha256(pin_bytes) != pin_digest:
            die("the base image's pinned blob is not {}: its download is corrupt".format(pin_digest))
        if pin_digest != manifest_digest:
            pin = json.loads(pin_bytes)
            listed = [
                m
                for m in pin.get("manifests", [])
                if m.get("digest") == manifest_digest and m.get("size") == len(manifest_bytes)
            ]
            if media_type(pin) not in (INDEX, DOCKER_INDEX) or not listed:
                die("the base manifest {} is not the pinned image {}".format(manifest_digest, pin_digest))

    config_bytes = read(args.base_config)
    config_desc = manifest.get("config", {})
    if sha256(config_bytes) != config_desc.get("digest") or len(config_bytes) != config_desc.get("size"):
        die("the base config does not match the base manifest")
    config = json.loads(config_bytes)
    if (config.get("os"), config.get("architecture")) != (settings["os"], settings["architecture"]):
        die("the base image is {}/{}, the image is {}/{}".format(
            config.get("os"),
            config.get("architecture"),
            settings["os"],
            settings["architecture"],
        ))

    layers = []
    for layer in manifest.get("layers", []):
        digest = check_digest(layer.get("digest"), "a base layer")
        oci_type = LAYER_TYPES.get(layer.get("mediaType"))
        if not oci_type:
            die("base layer {} is a {}, which is not supported".format(digest, layer.get("mediaType")))
        desc = {"mediaType": oci_type, "digest": digest, "size": layer["size"]}
        if layer.get("annotations"):
            desc["annotations"] = layer["annotations"]
        layers.append(desc)
    if len(config.get("rootfs", {}).get("diff_ids", [])) != len(layers):
        die("the base config's rootfs.diff_ids don't match its manifest's layers")
    return layers, config


def merge_env(base_env, env):
    merged = {}
    for item in base_env:
        key, _, value = item.partition("=")
        merged[key] = value
    merged.update(env)
    # Keep the base image's order, then new keys in sorted order.
    order = [item.partition("=")[0] for item in base_env]
    order += sorted(k for k in env if k not in order)
    return ["{}={}".format(k, merged[k]) for k in order]


def cmd_manifest(args):
    settings = read_json(args.settings)
    if args.base_manifest:
        layers, config = check_base(args, settings)
    else:
        layers = []
        config = {
            "architecture": settings["architecture"],
            "os": settings["os"],
            "config": {},
            "rootfs": {"type": "layers", "diff_ids": []},
            "history": [],
        }

    image_config = config.setdefault("config", {})
    for key in ("Entrypoint", "Cmd", "WorkingDir", "User"):
        if settings.get(key) is not None:
            image_config[key] = settings[key]
    if settings.get("Entrypoint") is not None and settings.get("Cmd") is None:
        # Like Dockerfile ENTRYPOINT: a new entrypoint resets the base's CMD.
        image_config.pop("Cmd", None)
    if settings.get("Env"):
        image_config["Env"] = merge_env(image_config.get("Env") or [], settings["Env"])
    if settings.get("ExposedPorts"):
        ports = dict(image_config.get("ExposedPorts") or {})
        for port in settings["ExposedPorts"]:
            ports[port if "/" in port else port + "/tcp"] = {}
        image_config["ExposedPorts"] = ports
    if settings.get("Labels"):
        labels = dict(image_config.get("Labels") or {})
        labels.update(settings["Labels"])
        image_config["Labels"] = labels

    history = config.setdefault("history", [])
    diff_ids = config.setdefault("rootfs", {"type": "layers", "diff_ids": []})["diff_ids"]
    for desc_path in args.layer or []:
        desc = read_json(desc_path)
        layers.append({"mediaType": desc["mediaType"], "digest": desc["digest"], "size": desc["size"]})
        diff_ids.append(desc["diffId"])
        history.append({"created": EPOCH, "created_by": "buck2 oci_image"})
    config["created"] = EPOCH

    config_bytes = canonical_json(config)
    write(args.out_config, config_bytes)
    manifest_bytes = canonical_json({
        "schemaVersion": 2,
        "mediaType": MANIFEST,
        "config": {"mediaType": CONFIG, "digest": sha256(config_bytes), "size": len(config_bytes)},
        "layers": layers,
    })
    write(args.out_manifest, manifest_bytes)
    manifest_digest = sha256(manifest_bytes)
    write(args.out_digest, (manifest_digest + "\n").encode())

    descriptor = {"mediaType": MANIFEST, "digest": manifest_digest, "size": len(manifest_bytes)}
    manifests = [
        dict(descriptor, annotations = {
            # `docker load` (containerd's image store) names the image after
            # this, OCI layout tools (skopeo `oci:DIR:TAG`, umoci) after that.
            "io.containerd.image.name": tag["name"],
            "org.opencontainers.image.ref.name": tag["tag"],
        })
        for tag in settings["tags"]
    ]
    write(args.out_index, canonical_json({"schemaVersion": 2, "mediaType": INDEX, "manifests": manifests or [descriptor]}))


# ---------------------------------------------------------------------------
# tarball


def tar_info(name, kind, mode, size = 0):
    info = tarfile.TarInfo(name)
    info.type = kind
    info.mode = mode
    info.size = size
    info.mtime = 0
    info.uid = info.gid = 0
    info.uname = info.gname = ""
    return info


class HashingReader:
    def __init__(self, f):
        self.f = f
        self.hash = hashlib.sha256()

    def read(self, n = -1):
        data = self.f.read(n)
        self.hash.update(data)
        return data


def cmd_tarball(args):
    def blob_path(digest):
        return os.path.join(args.layout, "blobs", "sha256", digest.partition(":")[2])

    index_bytes = read(os.path.join(args.layout, "index.json"))
    index = json.loads(index_bytes)
    digests = {check_digest(m.get("digest"), "an index.json entry") for m in index.get("manifests", [])}
    if len(digests) != 1:
        die("{} must hold one image, it has {}".format(args.layout, len(digests)))
    manifest_digest = digests.pop()
    manifest_bytes = read(blob_path(manifest_digest))
    if sha256(manifest_bytes) != manifest_digest:
        die("the manifest does not match its digest {}".format(manifest_digest))
    manifest = json.loads(manifest_bytes)
    config_digest = check_digest(manifest["config"]["digest"], "the config")
    layers = [check_digest(layer["digest"], "a layer") for layer in manifest["layers"]]
    sizes = {layer["digest"]: layer["size"] for layer in manifest["layers"]}
    sizes[config_digest] = manifest["config"]["size"]
    sizes[manifest_digest] = len(manifest_bytes)

    # Docker's classic image store loads manifest.json (as `docker save`
    # writes it); the containerd store and other tools load index.json.
    docker_manifest = canonical_json([{
        "Config": "blobs/sha256/" + config_digest.partition(":")[2],
        "Layers": ["blobs/sha256/" + d.partition(":")[2] for d in layers],
        "RepoTags": [
            m["annotations"]["io.containerd.image.name"]
            for m in index["manifests"]
            if "io.containerd.image.name" in m.get("annotations", {})
        ],
    }])
    files = {
        "oci-layout": b'{"imageLayoutVersion":"1.0.0"}',
        "index.json": index_bytes,
        "manifest.json": docker_manifest,
    }
    with open(args.out, "wb") as out, tarfile.open(
        fileobj = out,
        mode = "w",
        format = tarfile.PAX_FORMAT,
        encoding = "utf-8",
    ) as tar:
        tar.addfile(tar_info("blobs", tarfile.DIRTYPE, 0o755))
        tar.addfile(tar_info("blobs/sha256", tarfile.DIRTYPE, 0o755))
        for digest in sorted(sizes):
            path = blob_path(digest)
            size = os.path.getsize(path)
            if size != sizes[digest]:
                die("blob {} has {} bytes, expected {}".format(digest, size, sizes[digest]))
            with open(path, "rb") as f:
                reader = HashingReader(f)
                tar.addfile(tar_info("blobs/sha256/" + digest.partition(":")[2], tarfile.REGTYPE, 0o644, size), reader)
            if "sha256:" + reader.hash.hexdigest() != digest:
                die("blob {} does not match its digest".format(digest))
        for name in sorted(files):
            tar.addfile(tar_info(name, tarfile.REGTYPE, 0o644, len(files[name])), _BytesReader(files[name]))


class _BytesReader:
    def __init__(self, data):
        self.data = data
        self.pos = 0

    def read(self, n = -1):
        end = len(self.data) if n < 0 else self.pos + n
        data = self.data[self.pos:end]
        self.pos += len(data)
        return data


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest = "cmd", required = True)

    manifest = sub.add_parser("manifest")
    manifest.add_argument("--settings", required = True)
    manifest.add_argument("--base-manifest")
    manifest.add_argument("--base-config")
    manifest.add_argument("--base-pin", nargs = 2, metavar = ("DIGEST", "FILE"))
    manifest.add_argument("--layer", action = "append")
    manifest.add_argument("--out-manifest", required = True)
    manifest.add_argument("--out-config", required = True)
    manifest.add_argument("--out-index", required = True)
    manifest.add_argument("--out-digest", required = True)

    tarball = sub.add_parser("tarball")
    tarball.add_argument("--layout", required = True)
    tarball.add_argument("--out", required = True)

    args = parser.parse_args()
    {"manifest": cmd_manifest, "tarball": cmd_tarball}[args.cmd](args)


if __name__ == "__main__":
    main()
