#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

"""Points `<dest>/<name>` at each hook launcher built by `claude_hooks`.

Invoked through `buck2 run //:claude_hooks [-- --dest DIR]`, which passes
`--hook <name> <launcher>` for every hook, with the launcher as an absolute
path into buck-out.

Each symlink is replaced with a rename, so a hook that fires mid-install runs
either the previous launcher or the new one, never a missing or partial file.
"""

import argparse
import os


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--hook",
        nargs=2,
        action="append",
        default=[],
        metavar=("NAME", "LAUNCHER"),
    )
    parser.add_argument(
        "--dest",
        default=os.path.join(
            os.environ.get("CLAUDE_PROJECT_DIR", os.getcwd()), ".claude", "hooks"
        ),
    )
    args = parser.parse_args()

    os.makedirs(args.dest, exist_ok=True)
    hooks = dict(args.hook)
    for name, launcher in hooks.items():
        link = os.path.join(args.dest, name)
        tmp = os.path.join(args.dest, f".{name}.{os.getpid()}")
        # Relative, so the links survive moving the checkout.
        os.symlink(os.path.relpath(launcher, args.dest), tmp)
        os.replace(tmp, link)

    # The directory belongs to this installer, so any other symlink in it is a
    # hook that is no longer declared.
    for entry in os.scandir(args.dest):
        if entry.is_symlink() and entry.name not in hooks:
            os.unlink(entry.path)


if __name__ == "__main__":
    main()
