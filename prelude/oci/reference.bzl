# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Image references (`registry/repository:tag`), checked and normalized like
# Docker does (github.com/distribution/reference): `app` is
# `docker.io/library/app`, `me/app` is `docker.io/me/app`, and the tag
# defaults to `latest`.

_DOMAIN_COMPONENT = "(?:[a-zA-Z0-9]|[a-zA-Z0-9][a-zA-Z0-9-]*[a-zA-Z0-9])"
_DOMAIN = regex("^{c}(?:\\.{c})*(?::[0-9]+)?$".format(c = _DOMAIN_COMPONENT))
_PATH_COMPONENT = regex("^[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*$")
_TAG = regex("^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$")

def _split_domain(name: str) -> (str, str):
    first, sep, rest = name.partition("/")
    if sep and ("." in first or ":" in first or first == "localhost" or first.lower() != first):
        domain, path = first, rest
    else:
        domain, path = "docker.io", name
    if domain == "index.docker.io":
        domain = "docker.io"
    if domain == "docker.io" and "/" not in path:
        path = "library/" + path
    return domain, path

def _check_repository(what: str, ref: str, domain: str, path: str):
    if not _DOMAIN.match(domain):
        fail("{}: `{}` has an invalid registry `{}`".format(what, ref, domain))
    for component in path.split("/"):
        if not _PATH_COMPONENT.match(component):
            fail("{}: `{}` has an invalid path component `{}` (lowercase letters, digits and separators `.`, `_`, `__`, `-`)".format(what, ref, component))
    if len(domain) + 1 + len(path) > 255:
        fail("{}: `{}` is longer than 255 characters".format(what, ref))

def normalize_repository(what: str, ref: str) -> str:
    """`app` -> `docker.io/library/app`; fails on a tag or digest."""
    if "@" in ref or ":" in ref.rpartition("/")[2]:
        fail("{}: `{}` must be a repository, without a tag or digest".format(what, ref))
    domain, path = _split_domain(ref)
    _check_repository(what, ref, domain, path)
    return domain + "/" + path

def normalize_tagged(what: str, ref: str) -> struct:
    """`app:1` -> struct(name = "docker.io/library/app:1", tag = "1")."""
    if "@" in ref:
        fail("{}: `{}` must be a name and tag, without a digest".format(what, ref))
    name, tag = ref, "latest"
    if ":" in ref.rpartition("/")[2]:
        name, _, tag = ref.rpartition(":")
    if not _TAG.match(tag):
        fail("{}: `{}` has an invalid tag `{}`".format(what, ref, tag))
    domain, path = _split_domain(name)
    _check_repository(what, ref, domain, path)
    return struct(name = "{}/{}:{}".format(domain, path, tag), tag = tag)
