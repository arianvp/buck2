/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

// npm_node_modules' per-package action (prelude/js/npm/npm.bzl). Uses only
// Node.js built-ins. A file of its own so that editing the other npm tools
// doesn't change the key of every extract action.
//
//   npm_extract.cjs <tarball.tgz> <integrity> <out_dir>
//       Check the tarball against its Subresource Integrity hash, then
//       extract it like npm: strip the top-level directory, skip links and
//       rename .gitignore files to .npmignore. Files are 0755 if the tarball
//       has any executable bit, otherwise 0644 (npm keeps other bits, e.g.
//       0744, but buck2 and remote execution only keep the executable bit).

"use strict";

const crypto = require("crypto");
const fs = require("fs");
const path = require("path");
const zlib = require("zlib");

function die(msg) {
  process.stderr.write(`npm_extract: ${msg}\n`);
  process.exit(1);
}

// Same as in npm_fetch.cjs.
function checkIntegrity(data, integrity) {
  // An SRI string may list several hashes; any supported match is enough.
  const supported = ["sha512", "sha384", "sha256", "sha1"];
  const hashes = integrity
    .trim()
    .split(/\s+/)
    .map(h => {
      const i = h.indexOf("-");
      return {algo: h.slice(0, i), digest: h.slice(i + 1).split("?")[0]};
    })
    .filter(h => supported.includes(h.algo));
  if (hashes.length === 0) {
    return `no supported hash in integrity "${integrity}"`;
  }
  for (const {algo, digest} of hashes) {
    if (crypto.createHash(algo).update(data).digest("base64") === digest) {
      return null;
    }
  }
  return `integrity mismatch (expected ${integrity})`;
}

function parseOctal(buf, start, len) {
  const s = buf.toString("latin1", start, start + len).replace(/\0.*$/, "").trim();
  return s ? parseInt(s, 8) : 0;
}

function cString(buf, start, len) {
  const s = buf.toString("utf8", start, start + len);
  const nul = s.indexOf("\0");
  return nul === -1 ? s : s.slice(0, nul);
}

function parsePax(data) {
  const out = {};
  let i = 0;
  while (i < data.length) {
    const space = data.indexOf(0x20, i);
    const len = parseInt(data.toString("latin1", i, space), 10);
    const record = data.toString("utf8", space + 1, i + len - 1);
    const eq = record.indexOf("=");
    out[record.slice(0, eq)] = record.slice(eq + 1);
    i += len;
  }
  return out;
}

function main(tarball, integrity, outDir) {
  const tgz = fs.readFileSync(tarball);
  // npm_fetch.cjs checked it when downloading, but npm_download runs
  // locally and uploads its result to the shared cache, so the tarball
  // comes from whichever machine ran it. Check it again before using it.
  const err = checkIntegrity(tgz, integrity);
  if (err) {
    die(`${path.basename(tarball)}: ${err}`);
  }
  const tar = zlib.gunzipSync(tgz);
  fs.mkdirSync(outDir, {recursive: true});
  let offset = 0;
  let longName = null;
  const npmignores = new Set();
  while (offset + 512 <= tar.length) {
    const header = tar.subarray(offset, offset + 512);
    if (header.every(b => b === 0)) {
      break;
    }
    let size = parseOctal(header, 124, 12);
    let type = String.fromCharCode(header[156] || 0x30);

    if (type === "x" || type === "L" || type === "g") {
      const data = tar.subarray(offset + 512, offset + 512 + size);
      offset += 512 + Math.ceil(size / 512) * 512;
      if (type === "x") {
        longName = parsePax(data).path || longName;
      } else if (type === "L") {
        longName = cString(data, 0, data.length);
      }
      continue;
    }

    let name = cString(header, 0, 100);
    const prefix = header.toString("latin1", 257, 262) === "ustar" ? cString(header, 345, 155) : "";
    if (prefix) {
      name = prefix + "/" + name;
    }
    if (longName !== null) {
      name = longName;
      longName = null;
    }

    // Like node-tar (npm's tar): old tar versions wrote directories as
    // regular files whose name ends in "/", and some writers record a
    // directory's stat() size although directory entries have no body.
    if (type === "0" && name.endsWith("/")) {
      type = "5";
    }
    if (type === "5") {
      size = 0;
    }
    const data = tar.subarray(offset + 512, offset + 512 + size);
    offset += 512 + Math.ceil(size / 512) * 512;

    // Like npm: strip the first path component (usually "package/") and
    // ignore anything that is not a regular file or directory.
    const parts = name.split("/").filter(p => p !== "" && p !== ".");
    parts.shift();
    if (parts.length === 0 || parts.includes("..")) {
      continue;
    }
    let dest = path.join(outDir, ...parts);
    if (type === "5") {
      fs.mkdirSync(dest, {recursive: true, mode: 0o755});
    } else if (type === "0" || type === "7") {
      // Like pacote (npm's fetcher): a .gitignore becomes .npmignore,
      // unless the tarball had a .npmignore next to it before it.
      if (path.basename(dest) === ".npmignore") {
        npmignores.add(dest);
      } else if (path.basename(dest) === ".gitignore") {
        dest = path.join(path.dirname(dest), ".npmignore");
        if (npmignores.has(dest)) {
          continue;
        }
      }
      fs.mkdirSync(path.dirname(dest), {recursive: true, mode: 0o755});
      const exec = parseOctal(header, 100, 8) & 0o111;
      fs.writeFileSync(dest, data, {mode: exec ? 0o755 : 0o644});
      fs.chmodSync(dest, exec ? 0o755 : 0o644);
    }
  }
}

main(...process.argv.slice(2));
