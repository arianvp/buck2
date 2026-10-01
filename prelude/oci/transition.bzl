# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Container images are Linux images: oci_image and oci_pull switch the
# target platform's OS to Linux (keeping its CPU), so that the layers'
# contents are built for Linux whatever the host is (a Go binary is cross
# compiled, a toolchain archive is the Linux one) and the image's
# architecture is the platform's CPU. A Linux target platform is left as it
# is, so on Linux the image's dependencies share the configuration (and the
# cache) of building them on their own.

def _linux_impl(platform: PlatformInfo, refs: struct, attrs: struct) -> PlatformInfo:
    _ = attrs  # @unused
    linux = refs.linux[ConstraintValueInfo]
    constraints = platform.configuration.constraints
    if constraints.get(linux.setting.label) == linux:
        return platform
    constraints[linux.setting.label] = linux
    return PlatformInfo(
        label = platform.label + "-linux",
        configuration = ConfigurationInfo(
            constraints = constraints,
            values = platform.configuration.values,
        ),
    )

oci_linux_transition = transition(
    impl = _linux_impl,
    refs = {"linux": "config//os/constraints:linux"},
    attrs = [],
)
