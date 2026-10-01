# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

OciImageInfo = provider(
    fields = {
        # The image's config blob.
        "config": provider_field(Artifact),
        # An OCI image layout directory holding the image: its index.json
        # lists only this image's manifest (once per tag).
        "layout": provider_field(Artifact),
        # The image manifest blob.
        "manifest": provider_field(Artifact),
        # For a pulled image, what it was pinned to: struct(digest = "sha256:...",
        # blob = the manifest or index blob with that digest). Images built
        # on it check that `manifest` belongs to it.
        "pin": provider_field(typing.Any, default = None),
    },
)
