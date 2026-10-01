#!/usr/bin/env python3
"""Print a digest line per entry of one or more output trees.

    tree_digest.py PATH...

Each line is `<relative path> <kind> <x or -> <sha256 or symlink target>`,
in a deterministic order (each directory's entries sorted, as os.walk
visits them), so two builds' outputs can be compared with `diff`
(contents, files' executable bits and symlink targets; not timestamps or
other mode bits). The last line is a digest of all lines.
"""

import hashlib
import os
import stat
import sys


def entries(root):
    if os.path.isfile(root) or os.path.islink(root):
        yield from describe(root, os.path.basename(root))
        return
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for name in sorted(dirnames + filenames):
            path = os.path.join(dirpath, name)
            yield from describe(path, os.path.relpath(path, root))


def describe(path, rel):
    st = os.lstat(path)
    if stat.S_ISLNK(st.st_mode):
        yield f"{rel} link - {os.readlink(path)}"
    elif stat.S_ISDIR(st.st_mode):
        yield f"{rel} dir - -"
    else:
        with open(path, "rb") as f:
            digest = hashlib.sha256(f.read()).hexdigest()
        # Only the executable bit matters to CAS-based execution.
        yield f"{rel} file {'x' if st.st_mode & 0o111 else '-'} {digest}"


def main():
    lines = []
    for root in sys.argv[1:]:
        lines.extend(f"{os.path.basename(root.rstrip('/'))}/{e}" for e in entries(root))
    for line in lines:
        print(line)
    print("TOTAL " + hashlib.sha256("\n".join(lines).encode()).hexdigest())


if __name__ == "__main__":
    main()
