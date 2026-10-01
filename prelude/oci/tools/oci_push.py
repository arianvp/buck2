# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

"""Push an OCI image layout (holding one image manifest) to a registry.

    oci_push.py --layout DIR --repository REGISTRY/REPOSITORY [--tag TAG]...
        [--insecure-registry HOST]... [ARG...]

Extra arguments (from `buck2 run //:push -- ARG...`) are more `--tag TAG`s,
`--repository` to push somewhere else, or `--insecure-registry`. Blobs the
registry already has are skipped. Prints `REPOSITORY@DIGEST`.

Credentials come from the Docker config ($DOCKER_CONFIG/config.json or
~/.docker/config.json): `auths` entries and credential helpers
(`credHelpers`/`credsStore`, run as `docker-credential-<name> get`). They
are only sent to the registry itself (same scheme, host and port) and, when
the registry asks for a token, to the token server it names; blob uploads
that redirect to other hosts (storage backends) don't get them. An
`identitytoken` (an OAuth2 refresh token) is sent like a password, so
registries that require the OAuth2 token flow for it don't accept it. Only HTTPS
is used, unless the registry's host (e.g. `localhost:5000`, a local test
registry) is one of the `--insecure-registry` hosts: then that registry, its
token server and redirects may use plain HTTP.

Each blob is checked against its digest before it is uploaded, and
uploaded from disk without reading it into memory.
"""

import argparse
import base64
import hashlib
import json
import os
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request

MANIFEST_TYPES = (
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
)


def die(msg):
    sys.stderr.write("oci_push: {}\n".format(msg))
    sys.exit(1)


def parse_repository(ref):
    first, sep, rest = ref.partition("/")
    if sep and ("." in first or ":" in first or first == "localhost"):
        registry, repository = first, rest
    else:
        registry, repository = "docker.io", ref
    if ":" in repository.rsplit("/", 1)[-1] or "@" in repository:
        die("--repository must not contain a tag or digest: {}".format(ref))
    if registry in ("docker.io", "index.docker.io", "registry-1.docker.io"):
        if "/" not in repository:
            repository = "library/" + repository
        return "registry-1.docker.io", "index.docker.io", repository
    return registry, registry, repository


def docker_credentials(config_key):
    """(user, secret) for a registry from the Docker config, or None."""
    config_dir = os.environ.get("DOCKER_CONFIG") or os.path.join(os.path.expanduser("~"), ".docker")
    try:
        with open(os.path.join(config_dir, "config.json"), encoding = "utf-8") as f:
            config = json.load(f)
    except FileNotFoundError:
        return None
    keys = [config_key, "https://" + config_key, "https://{}/v1/".format(config_key)]
    helper = None
    for key in keys:
        helper = (config.get("credHelpers") or {}).get(key) or helper
    helper = helper or config.get("credsStore")
    for key in keys:
        auth = (config.get("auths") or {}).get(key) or {}
        if auth.get("auth"):
            user, _, secret = base64.b64decode(auth["auth"]).decode().partition(":")
            return user, secret
        if auth.get("identitytoken"):
            return "<token>", auth["identitytoken"]
    if helper:
        for key in keys:
            res = subprocess.run(
                ["docker-credential-" + helper, "get"],
                input = key.encode(),
                capture_output = True,
            )
            if res.returncode == 0:
                creds = json.loads(res.stdout)
                return creds["Username"], creds["Secret"]
    return None


def _opener():
    # No file:, ftp: or data: handlers, and no automatic redirects (they are
    # followed by hand, keeping credentials to the registry).
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


_opener = _opener()


def _origin(url):
    u = urllib.parse.urlsplit(url)
    return (u.scheme, u.hostname, u.port or {"http": 80, "https": 443}.get(u.scheme))


class Registry:
    def __init__(self, host, config_key, repository, insecure):
        self.scheme = "http" if insecure else "https"
        self.base = "{}://{}".format(self.scheme, host)
        self.origin = _origin(self.base + "/")
        self.host = host
        self.repository = repository
        self.credentials = docker_credentials(config_key)
        self.authorization = None

    def check_url(self, url):
        if urllib.parse.urlsplit(url).scheme not in ("https", self.scheme):
            die("refusing to send a request to {}: only https is used".format(url))

    def _authenticate(self, challenge):
        scheme, _, params = challenge.partition(" ")
        if scheme.lower() == "basic":
            if not self.credentials:
                die("{} needs credentials; none found in the Docker config".format(self.host))
            self.authorization = "Basic " + base64.b64encode(":".join(self.credentials).encode()).decode()
            return
        fields = {}
        for part in params.split(","):
            key, _, value = part.strip().partition("=")
            fields[key] = value.strip('"')
        realm = fields.get("realm", "")
        self.check_url(realm)
        query = {"scope": "repository:{}:pull,push".format(self.repository)}
        if "service" in fields:
            query["service"] = fields["service"]
        req = urllib.request.Request(realm + ("&" if "?" in realm else "?") + urllib.parse.urlencode(query))
        if self.credentials:
            req.add_header("Authorization", "Basic " + base64.b64encode(":".join(self.credentials).encode()).decode())
        try:
            with _opener.open(req, timeout = 60) as res:
                body = json.loads(res.read(1 << 20))
        except urllib.error.HTTPError as e:
            die("token server {}: HTTP {} {}".format(realm, e.code, e.read()[:500].decode(errors = "replace")))
        except (urllib.error.URLError, ValueError) as e:
            die("token server {}: {}".format(realm, getattr(e, "reason", e)))
        token = (body.get("token") or body.get("access_token")) if isinstance(body, dict) else None
        if not isinstance(token, str) or not token:
            die("{} returned no token".format(realm))
        self.authorization = "Bearer " + token

    def request(self, method, url, data = None, headers = None):
        """Returns (status, headers, body). Follows redirects, sending the
        registry's credentials only to the registry itself. `data` is bytes
        or a function returning a new file object to upload."""
        if url.startswith("/"):
            url = self.base + url
        reauthenticated = False
        for _ in range(10):
            body = data() if callable(data) else data
            req = urllib.request.Request(url, data = body, method = method, headers = dict(headers or {}))
            if self.authorization and _origin(url) == self.origin:
                req.add_header("Authorization", self.authorization)
            try:
                with _opener.open(req, timeout = 300) as res:
                    return res.status, res.headers, res.read()
            except urllib.error.HTTPError as e:
                if e.code == 401 and "WWW-Authenticate" in e.headers and _origin(url) == self.origin and not reauthenticated:
                    # First request, or an expired token.
                    reauthenticated = self.authorization is not None
                    self._authenticate(e.headers["WWW-Authenticate"])
                    continue
                if e.code in (301, 302, 303, 307, 308) and "Location" in e.headers:
                    url = urllib.parse.urljoin(url, e.headers["Location"])
                    self.check_url(url)
                    continue
                if method == "HEAD" and e.code == 404:
                    return 404, e.headers, b""
                die("{} {}: HTTP {} {}".format(method, url, e.code, e.read()[:500].decode(errors = "replace")))
            except urllib.error.URLError as e:
                die("{} {}: {}".format(method, url, e.reason))
            finally:
                if hasattr(body, "close"):
                    body.close()
        die("too many redirects for {}".format(url))

    def has_blob(self, digest):
        status, _, _ = self.request("HEAD", "/v2/{}/blobs/{}".format(self.repository, digest))
        return status == 200

    def push_blob(self, digest, size, path):
        if self.has_blob(digest):
            return False
        check_blob(digest, size, path)
        _, headers, _ = self.request("POST", "/v2/{}/blobs/uploads/".format(self.repository), data = b"")
        location = urllib.parse.urljoin(self.base + "/", headers["Location"])
        self.check_url(location)
        sep = "&" if "?" in location else "?"
        self.request(
            "PUT",
            location + sep + urllib.parse.urlencode({"digest": digest}),
            data = lambda: open(path, "rb"),
            headers = {"Content-Type": "application/octet-stream", "Content-Length": str(size)},
        )
        return True

    def push_manifest(self, reference, media_type, data):
        self.request(
            "PUT",
            "/v2/{}/manifests/{}".format(self.repository, reference),
            data = data,
            headers = {"Content-Type": media_type},
        )


def check_blob(digest, size, path):
    h = hashlib.sha256()
    actual = 0
    with open(path, "rb") as f:
        while True:
            chunk = f.read(1 << 20)
            if not chunk:
                break
            h.update(chunk)
            actual += len(chunk)
    if "sha256:" + h.hexdigest() != digest or actual != size:
        die("{} in the image layout is corrupt".format(digest))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--layout", required = True)
    parser.add_argument("--repository", action = "append", required = True)
    parser.add_argument("--tag", action = "append", default = [])
    parser.add_argument("--insecure-registry", action = "append", default = [])
    args = parser.parse_args()
    # The last --repository wins, so `buck2 run //:push -- --repository X` overrides the rule's.
    repository_ref = args.repository[-1]

    with open(os.path.join(args.layout, "index.json"), encoding = "utf-8") as f:
        index = json.load(f)
    if len({m.get("digest") for m in index.get("manifests", [])}) != 1:
        die("the layout must hold exactly one image")
    descriptor = index["manifests"][0]
    if descriptor["mediaType"] not in MANIFEST_TYPES:
        die("{} is not an image manifest".format(descriptor["mediaType"]))
    digest = descriptor["digest"]

    def blob(d):
        return os.path.join(args.layout, "blobs", *d.split(":", 1))

    if not digest.startswith("sha256:"):
        die("unsupported digest {}".format(digest))
    with open(blob(digest), "rb") as f:
        manifest_bytes = f.read()
    if "sha256:" + hashlib.sha256(manifest_bytes).hexdigest() != digest:
        die("{} in the image layout is corrupt".format(digest))
    manifest = json.loads(manifest_bytes)

    host, config_key, repository = parse_repository(repository_ref)
    insecure = bool({host, config_key} & set(args.insecure_registry))
    registry = Registry(host, config_key, repository, insecure = insecure)
    for item in manifest["layers"] + [manifest["config"]]:
        uploaded = registry.push_blob(item["digest"], item["size"], blob(item["digest"]))
        sys.stderr.write("{} {}\n".format("pushed" if uploaded else "exists", item["digest"]))
    registry.push_manifest(digest, descriptor["mediaType"], manifest_bytes)
    for tag in args.tag:
        registry.push_manifest(tag, descriptor["mediaType"], manifest_bytes)
    print("{}@{}".format(repository_ref, digest))


if __name__ == "__main__":
    main()
