# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

"""Download a container image pinned by digest into an OCI image layout.

    oci_pull.py --image REGISTRY/REPOSITORY --digest sha256:... \\
        --os linux --arch amd64 [--variant v8] [--insecure] --out DIR \\
        --out-manifest FILE --out-config FILE --out-pinned FILE

The digest names either an image manifest or an index (a multi-platform
image); for an index, the manifest for --os/--arch/--variant is used. Every
download is streamed to disk, limited to the size its descriptor gives (4
MiB for manifests), and checked against its sha256 digest, so the output
only depends on the arguments. Registries that need a token for anonymous
pulls (Docker Hub, ghcr.io, ...) are supported; credentials are not.

DIR gets an OCI image layout (oci-layout, index.json, blobs/sha256/...)
holding exactly one image manifest, its config and its layers; the
manifest, the config and the pinned blob (the manifest, or the index) are
also copied to the --out-* files.

Only HTTPS is used (plain HTTP everywhere with --insecure, for a local test
registry): for the registry, token servers and redirects. The registry's
token is only sent to the registry itself, not to the storage hosts that
blob downloads redirect to.
"""

import argparse
import hashlib
import http.client
import json
import os
import shutil
import ssl
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

INDEX_TYPES = {
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
}
MANIFEST_TYPES = {
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
}
ACCEPT = ", ".join(sorted(INDEX_TYPES | MANIFEST_TYPES))
# The distribution spec lets registries refuse larger manifests.
MAX_MANIFEST_SIZE = 4 << 20
CHUNK = 1 << 20


def die(msg):
    sys.stderr.write("oci_pull: {}\n".format(msg))
    sys.exit(1)


class HTTPStatus(Exception):
    def __init__(self, code, headers):
        super().__init__("HTTP {}".format(code))
        self.code = code
        self.headers = headers


def parse_image(image):
    """Returns (registry host, repository), with Docker Hub's conventions."""
    first, sep, rest = image.partition("/")
    if sep and ("." in first or ":" in first or first == "localhost"):
        registry, repository = first, rest
    else:
        registry, repository = "docker.io", image
    if registry in ("docker.io", "index.docker.io"):
        registry = "registry-1.docker.io"
        if "/" not in repository:
            repository = "library/" + repository
    if "@" in repository or ":" in repository.rsplit("/", 1)[-1]:
        die("--image must not contain a tag or digest: {}".format(image))
    return registry, repository


def _opener():
    # No file:, ftp: or data: handlers, and no automatic redirects: a
    # registry must not make this read local files or leave HTTPS.
    opener = urllib.request.OpenerDirector()
    for handler in (
        urllib.request.ProxyHandler(),
        urllib.request.UnknownHandler(),
        urllib.request.HTTPHandler(),
        urllib.request.HTTPSHandler(),
        urllib.request.HTTPDefaultErrorHandler(),
        urllib.request.HTTPErrorProcessor(),
    ):
        opener.add_handler(handler)
    return opener


_OPENER = _opener()


def _origin(url):
    u = urllib.parse.urlsplit(url)
    return (u.scheme, u.hostname, u.port or {"http": 80, "https": 443}.get(u.scheme))


class Registry:
    def __init__(self, host, repository, insecure):
        self.scheme = "http" if insecure else "https"
        self.origin = _origin("{}://{}/".format(self.scheme, host))
        self.base = "{}://{}/v2/{}/".format(self.scheme, host, repository)
        self.repository = repository
        self.token = None

    def check_url(self, url):
        if urllib.parse.urlsplit(url).scheme not in ("https", self.scheme):
            die("refusing to fetch {}: only https is used".format(url))

    def _authenticate(self, challenge):
        # WWW-Authenticate: Bearer realm="...",service="...",scope="..."
        scheme, _, params = challenge.partition(" ")
        if scheme.lower() != "bearer":
            die("the registry wants {} authentication, which is not supported".format(scheme))
        fields = {}
        for part in params.split(","):
            key, _, value = part.strip().partition("=")
            fields[key] = value.strip('"')
        realm = fields.get("realm", "")
        self.check_url(realm)
        query = {"scope": fields.get("scope", "repository:{}:pull".format(self.repository))}
        if "service" in fields:
            query["service"] = fields["service"]
        url = realm + ("&" if "?" in realm else "?") + urllib.parse.urlencode(query)
        with _OPENER.open(urllib.request.Request(url, headers = {"User-Agent": "buck2-oci-pull"}), timeout = 60) as res:
            body = json.loads(res.read(1 << 20))
        token = body.get("token") or body.get("access_token") if isinstance(body, dict) else None
        if not isinstance(token, str) or not token:
            die("{} returned no token".format(realm))
        self.token = token

    def open(self, path, accept = None):
        """Opens <registry>/v2/<repository>/<path>, authenticating when asked
        to and following redirects (sending the token only to the registry)."""
        url = self.base + path
        reauthenticated = False
        for _ in range(10):
            headers = {"User-Agent": "buck2-oci-pull"}
            if accept:
                headers["Accept"] = accept
            if self.token and _origin(url) == self.origin:
                headers["Authorization"] = "Bearer " + self.token
            try:
                return _OPENER.open(urllib.request.Request(url, headers = headers), timeout = 120)
            except urllib.error.HTTPError as e:
                e.close()
                if e.code == 401 and "WWW-Authenticate" in e.headers and _origin(url) == self.origin and not reauthenticated:
                    # First request, or an expired token (Docker Hub's last 5 minutes).
                    reauthenticated = self.token is not None
                    self._authenticate(e.headers["WWW-Authenticate"])
                    continue
                if e.code in (301, 302, 303, 307, 308) and "Location" in e.headers:
                    url = urllib.parse.urljoin(url, e.headers["Location"])
                    self.check_url(url)
                    continue
                raise HTTPStatus(e.code, e.headers)
        die("too many redirects for {}".format(path))

    def download(self, path, digest, max_size, out, accept = None):
        """Streams <path> to the file `out`, checking it against `digest` and
        stopping after `max_size` bytes."""
        h = hashlib.sha256()
        size = 0
        with self.open(path, accept) as res, open(out, "wb") as f:
            while True:
                chunk = res.read(CHUNK)
                if not chunk:
                    break
                size += len(chunk)
                if size > max_size:
                    die("{}: more than the expected {} bytes".format(digest, max_size))
                h.update(chunk)
                f.write(chunk)
        if "sha256:" + h.hexdigest() != digest:
            die("{}: the content has digest sha256:{}".format(digest, h.hexdigest()))
        return size


def with_retries(what, fn):
    for attempt in range(6):
        try:
            return fn()
        except HTTPStatus as e:
            if e.code not in (429, 500, 502, 503, 504) or attempt == 5:
                die("{}: HTTP {}".format(what, e.code))
            delay = 2 ** attempt
            retry_after = e.headers.get("Retry-After", "")
            if retry_after.isdigit():
                delay = min(int(retry_after), 120)
        except (urllib.error.URLError, http.client.HTTPException, OSError) as e:
            # TLS errors (e.g. a certificate that doesn't verify) don't go away.
            if attempt == 5 or isinstance(getattr(e, "reason", e), ssl.SSLError):
                die("{}: {}".format(what, e))
            delay = 2 ** attempt
        time.sleep(delay)


def media_type(doc):
    # `mediaType` is optional in OCI manifests and indexes: never trust the
    # Content-Type header, which differs between registries and mirrors.
    if doc.get("mediaType"):
        return doc["mediaType"]
    return "application/vnd.oci.image.index.v1+json" if "manifests" in doc else "application/vnd.oci.image.manifest.v1+json"


def check_digest(digest, what):
    algo, _, hex_digest = str(digest).partition(":")
    if algo != "sha256" or len(hex_digest) != 64 or any(c not in "0123456789abcdef" for c in hex_digest):
        die("{} has an unsupported digest {!r} (only sha256 is supported)".format(what, digest))
    return digest


def platform_matches(platform, want_os, want_arch, want_variant):
    if platform.get("os") != want_os or platform.get("architecture") != want_arch:
        return False
    return not want_variant or platform.get("variant") == want_variant


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--image", required = True)
    parser.add_argument("--digest", required = True)
    parser.add_argument("--os", required = True)
    parser.add_argument("--arch", required = True)
    parser.add_argument("--variant", default = "")
    parser.add_argument("--insecure", action = "store_true")
    parser.add_argument("--out", required = True)
    parser.add_argument("--out-manifest", required = True)
    parser.add_argument("--out-config", required = True)
    parser.add_argument("--out-pinned", required = True)
    args = parser.parse_args()

    registry = Registry(*parse_image(args.image), insecure = args.insecure)
    blobs = os.path.join(args.out, "blobs", "sha256")
    os.makedirs(blobs, exist_ok = True)

    def fetch(kind, digest, max_size, accept = None):
        path = os.path.join(blobs, check_digest(digest, kind).partition(":")[2])
        size = with_retries(digest, lambda: registry.download("{}/{}".format(kind, digest), digest, max_size, path, accept))
        return path, size

    pinned_path, pinned_size = fetch("manifests", args.digest, MAX_MANIFEST_SIZE, ACCEPT)
    with open(pinned_path, "rb") as f:
        doc = json.load(f)
    shutil.copyfile(pinned_path, args.out_pinned)
    digest, path, size = args.digest, pinned_path, pinned_size

    if media_type(doc) in INDEX_TYPES:
        candidates = [
            m
            for m in doc.get("manifests", [])
            if platform_matches(m.get("platform", {}), args.os, args.arch, args.variant)
        ]
        if len(candidates) != 1:
            available = sorted({
                "{}/{}{}".format(
                    m.get("platform", {}).get("os"),
                    m.get("platform", {}).get("architecture"),
                    "/" + m["platform"]["variant"] if m.get("platform", {}).get("variant") else "",
                )
                for m in doc.get("manifests", [])
            })
            die("{} has {} manifests for {}/{}{} (available: {})".format(
                args.digest,
                len(candidates),
                args.os,
                args.arch,
                "/" + args.variant if args.variant else "",
                ", ".join(available),
            ))
        descriptor = candidates[0]
        if descriptor.get("mediaType") not in MANIFEST_TYPES:
            die("{} lists a {} for {}/{}, not an image manifest".format(args.digest, descriptor.get("mediaType"), args.os, args.arch))
        if not isinstance(descriptor.get("size"), int) or descriptor["size"] > MAX_MANIFEST_SIZE:
            die("{} lists a manifest of {} bytes".format(args.digest, descriptor.get("size")))
        digest = descriptor["digest"]
        path, size = fetch("manifests", digest, descriptor["size"], ACCEPT)
        if size != descriptor["size"]:
            die("{}: {} bytes, expected {}".format(digest, size, descriptor["size"]))
        os.remove(pinned_path)  # Only the image's blobs go in the layout.
        with open(path, "rb") as f:
            doc = json.load(f)
    if media_type(doc) not in MANIFEST_TYPES:
        die("{} is a {}, not an image manifest or index".format(digest, media_type(doc)))
    shutil.copyfile(path, args.out_manifest)

    for i, blob in enumerate([doc["config"]] + doc.get("layers", [])):
        if not isinstance(blob.get("size"), int):
            die("{}: a blob has no size".format(digest))
        blob_path, blob_size = fetch("blobs", blob.get("digest"), blob["size"])
        if blob_size != blob["size"]:
            die("blob {}: {} bytes, expected {}".format(blob["digest"], blob_size, blob["size"]))
        if i == 0:
            shutil.copyfile(blob_path, args.out_config)

    with open(os.path.join(args.out, "oci-layout"), "w") as f:
        f.write('{"imageLayoutVersion":"1.0.0"}')
    index = {
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{"mediaType": media_type(doc), "digest": digest, "size": size}],
    }
    with open(os.path.join(args.out, "index.json"), "w") as f:
        json.dump(index, f, sort_keys = True, separators = (",", ":"))


if __name__ == "__main__":
    main()
