# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

"""Write one reproducible OCI image layer.

    oci_layer.py --entries ENTRIES.json [--compress] --out-blob FILE --out-desc FILE

ENTRIES.json is a list of {"dest": "/abs/path/in/image", "src": PATH}. A file
becomes dest; a directory's contents go under dest ("/" for the root).
Writes the layer (a tar, gzipped with --compress) and a JSON descriptor
{mediaType, digest, size, diffId}.

The tar is reproducible: entries are sorted, timestamps are 0, owners are
root (uid/gid 0), and modes are 0755 for directories and executables and
0644 for other files. Only what the sources contain is written: the
destinations' parent directories (and a directory source's own
destination) are not, so that existing directories of the base image, and
symlinks such as /bin -> usr/bin, keep their type, owner and mode; runtimes
create missing parents as root-owned 0755 directories, like Dockerfile COPY.

Symlinks inside a directory source are kept when they are relative and stay
inside that directory, or are absolute and point outside the build (they
then point into the image). Others (links out of the directory, such as
buck2's symlinked_dir outputs, or absolute links into the build) are
replaced by what they point to.

gzip output depends on the zlib implementation, so --compress first checks
that this Python's zlib compresses a test input byte for byte like the
reference zlib (1.2.x and 1.3.x all do; zlib-ng, the default on Fedora 40+,
does not) and fails otherwise, instead of writing a layer whose digest
differs from every other machine's.

This file only imports the standard library and nothing that changes the
layer's bytes is read from elsewhere, so its own content and the inputs
determine the output (run it with `python3 -I -B -X utf8`).
"""

import argparse
import gzip
import hashlib
import json
import os
import posixpath
import stat
import sys
import tarfile
import zlib

LAYER_TAR = "application/vnd.oci.image.layer.v1.tar"
LAYER_TAR_GZIP = "application/vnd.oci.image.layer.v1.tar+gzip"

# sha256 of zlib's raw deflate (level 6, the default window and memLevel) of
# _zlib_test_input(), as zlib 1.2.13, 1.3, 1.3.1 and 1.3.2 produce it.
_REFERENCE_DEFLATE = "cc4ed1199f2857de213b5ba597cb1594d330251aa4eaf440a7e8d8c7e3929f3a"


def die(msg):
    sys.stderr.write("oci_layer: {}\n".format(msg))
    sys.exit(1)


def _zlib_test_input():
    # 64 KiB of deterministic, compressible bytes: words picked by an LCG.
    words = [b"layer", b"image", b"buck2", b"oci", b"tar", b"gzip", b"digest", b"sha256", b"node_modules", b"\x00\x01", b"/usr/lib/", b"0123456789"]
    x, out = 1, bytearray()
    while len(out) < 1 << 16:
        x = (x * 1103515245 + 12345) & 0x7FFFFFFF
        out += words[x % len(words)]
        out.append(x >> 16 & 0xFF)
    return bytes(out)


def check_reference_zlib():
    # The same parameters as gzip.GzipFile.
    c = zlib.compressobj(6, zlib.DEFLATED, -zlib.MAX_WBITS, zlib.DEF_MEM_LEVEL, 0)
    data = c.compress(_zlib_test_input()) + c.flush()
    if hashlib.sha256(data).hexdigest() != _REFERENCE_DEFLATE:
        die(
            "this Python's zlib ({}) does not compress like the reference zlib, so the "
            "layer's digest (and the image's) would differ from other machines'. Build it "
            "with another python3 (e.g. on remote execution), or set `compress = False`.".format(
                zlib.ZLIB_RUNTIME_VERSION,
            ),
        )


def _inside(path, root):
    return path == root or path.startswith(root.rstrip(os.sep) + os.sep)


def _stays_in_tree(depth, target):
    """Whether a relative symlink `depth` directories below a tree's root
    resolves inside the tree without ever leaving it (`../../<root's name>`
    would not resolve to the same place once the tree is in the image)."""
    for part in target.split("/"):
        if part == "..":
            depth -= 1
            if depth < 0:
                return False
        elif part not in ("", "."):
            depth += 1
    return True


class Layer:
    """The layer's entries: {path in the image without the leading "/":
    ("dir",) | ("file", source path) | ("symlink", target)}."""

    def __init__(self):
        self.entries = {}
        # Symlinks may only be followed to files in the build: on remote
        # execution, the action's inputs are all under its working directory.
        self.build_root = os.path.realpath(os.getcwd())

    def add(self, path, entry, src):
        for part in path.split("/"):
            if part.startswith(".wh."):
                die("/{} (from {}): names starting with `.wh.` mark deletions in image layers".format(path, self.show(src)))
        if path in self.entries:
            die("/{} is in the layer twice (the second time from {})".format(path, self.show(src)))
        self.entries[path] = entry

    def show(self, path):
        """`path` for messages: relative to the build's root if it is in it."""
        real = os.path.realpath(path)
        return os.path.relpath(path, self.build_root) if _inside(real, self.build_root) else path

    def resolve(self, path):
        real = os.path.realpath(path)
        if not os.path.exists(real):
            die("{} is a dangling symlink".format(self.show(path)))
        if not _inside(real, self.build_root):
            die("{} resolves to {}, which is not in the build".format(self.show(path), real))
        return real

    def add_source(self, dest, src):
        path = posixpath.normpath(dest).lstrip("/")
        # An artifact itself can be a symlink (e.g. into a downloaded archive).
        real = self.resolve(src)
        if os.path.isdir(real):
            self.add_tree(path, real, (real,))
        elif not path:
            die("{} is a file, it can't be the image's root directory".format(src))
        else:
            self.add(path, ("file", real), src)

    def add_tree(self, base, root, chain):
        """Adds the contents of the directory `root` under `base`."""
        for dir_path, dirs, names in os.walk(root):
            dirs.sort()
            rel_dir = os.path.relpath(dir_path, root)
            rel_dir = "" if rel_dir == "." else rel_dir.replace(os.sep, "/")
            depth = len(rel_dir.split("/")) if rel_dir else 0
            # os.walk lists symlinks to directories in `dirs` but does not
            # descend into them.
            for name in sorted(dirs + names):
                src = os.path.join(dir_path, name)
                path = posixpath.join(base, rel_dir, name)
                if os.path.islink(src):
                    target = os.readlink(src)
                    if os.path.isabs(target):
                        keep = not (
                            _inside(os.path.realpath(target), self.build_root) or
                            _inside(os.path.normpath(target), os.path.normpath(os.getcwd()))
                        )
                    else:
                        keep = _stays_in_tree(depth, target)
                    if keep:
                        self.add(path, ("symlink", target), src)
                        continue
                    real = self.resolve(src)
                    if os.path.isdir(real):
                        if real in chain:
                            die("{} is a symlink loop".format(self.show(src)))
                        self.add(path, ("dir",), src)
                        self.add_tree(path, real, chain + (real,))
                    else:
                        self.add(path, ("file", real), src)
                elif os.path.isdir(src):
                    self.add(path, ("dir",), src)
                else:
                    self.add(path, ("file", src), src)

    def check(self):
        # Extracting a path whose parent is a file or a symlink would fail,
        # or write through the symlink (e.g. into the base image's /etc).
        for path in self.entries:
            parent = posixpath.dirname(path)
            while parent:
                entry = self.entries.get(parent)
                if entry and entry[0] != "dir":
                    die("/{} is in the layer, but /{} is a {}".format(path, parent, entry[0]))
                parent = posixpath.dirname(parent)


def tar_info(name, kind, mode):
    info = tarfile.TarInfo(name)
    info.type = kind
    info.mode = mode
    info.mtime = 0
    info.uid = info.gid = 0
    info.uname = info.gname = ""
    return info


def write_tar(layer, out):
    with tarfile.open(
        fileobj = out,
        mode = "w",
        format = tarfile.PAX_FORMAT,
        encoding = "utf-8",
        errors = "surrogateescape",
    ) as tar:
        # Sorted, so directories come before their contents.
        for path in sorted(layer.entries):
            entry = layer.entries[path]
            if entry[0] == "dir":
                tar.addfile(tar_info(path, tarfile.DIRTYPE, 0o755))
            elif entry[0] == "symlink":
                info = tar_info(path, tarfile.SYMTYPE, 0o777)
                info.linkname = entry[1]
                tar.addfile(info)
            else:
                # Checked before opening: opening a FIFO would block.
                if not stat.S_ISREG(os.stat(entry[1]).st_mode):
                    die("{} is not a regular file, directory or symlink".format(layer.show(entry[1])))
                with open(entry[1], "rb") as f:
                    st = os.fstat(f.fileno())
                    info = tar_info(path, tarfile.REGTYPE, 0o755 if st.st_mode & 0o111 else 0o644)
                    info.size = st.st_size
                    tar.addfile(info, f)


class HashingWriter:
    """A file-like object that hashes what it writes and passes it on."""

    def __init__(self, out):
        self.out = out
        self.hash = hashlib.sha256()
        self.size = 0

    def write(self, data):
        self.hash.update(data)
        self.size += len(data)
        return self.out.write(data)

    def flush(self):
        self.out.flush()

    def tell(self):
        return self.size

    def digest(self):
        return "sha256:" + self.hash.hexdigest()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--entries", required = True)
    parser.add_argument("--compress", action = "store_true")
    parser.add_argument("--out-blob", required = True)
    parser.add_argument("--out-desc", required = True)
    args = parser.parse_args()

    with open(args.entries, encoding = "utf-8") as f:
        entries = json.load(f)
    layer = Layer()
    for entry in entries:
        layer.add_source(entry["dest"], entry["src"])
    layer.check()
    if args.compress:
        check_reference_zlib()

    # One pass, without holding the layer in memory: the tar is hashed (its
    # diff ID), then gzipped, and the gzip stream is hashed (its digest).
    with open(args.out_blob, "wb") as out:
        blob = HashingWriter(out)
        if args.compress:
            # No file name and mtime 0 in the gzip header.
            gz = gzip.GzipFile(filename = "", mode = "wb", fileobj = blob, mtime = 0, compresslevel = 6)
            tar = HashingWriter(gz)
            write_tar(layer, tar)
            gz.close()
            media_type = LAYER_TAR_GZIP
        else:
            tar = blob
            write_tar(layer, tar)
            media_type = LAYER_TAR
    desc = {"mediaType": media_type, "digest": blob.digest(), "size": blob.size, "diffId": tar.digest()}
    with open(args.out_desc, "w", encoding = "utf-8") as f:
        json.dump(desc, f, sort_keys = True)


if __name__ == "__main__":
    main()
