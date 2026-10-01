/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

// npm_download's action (prelude/js/npm/npm.bzl). Uses only Node.js
// built-ins. A file of its own so that editing the other npm tools doesn't
// change this action's key: a changed key means downloading every tarball
// again on machines without the previous output.
//
//   npm_fetch.cjs <manifest.json> <out_dir>
//       Download every {url, integrity, name} in the manifest to
//       <out_dir>/<name>, verifying the Subresource Integrity hash. Files
//       already present from a previous run are reused if they still verify,
//       and files not in the manifest are removed.

"use strict";

const crypto = require("crypto");
const fs = require("fs");
const path = require("path");

function die(msg) {
  process.stderr.write(`npm_fetch: ${msg}\n`);
  process.exit(1);
}

// Same as in npm_extract.cjs.
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

async function fetchOne(entry, outDir) {
  const dest = path.join(outDir, entry.name);
  // Reuse a tarball left by a previous run (the action is incremental) if it
  // still matches the lockfile.
  if (fs.existsSync(dest) && checkIntegrity(fs.readFileSync(dest), entry.integrity) === null) {
    return;
  }
  let lastErr;
  for (let attempt = 1; attempt <= 4; attempt++) {
    try {
      const res = await fetch(entry.url);
      if (!res.ok) {
        throw new Error(`HTTP ${res.status}`);
      }
      const data = Buffer.from(await res.arrayBuffer());
      const err = checkIntegrity(data, entry.integrity);
      if (err) {
        // Not retryable: the registry served different bytes than the lockfile expects.
        die(`${entry.url}: ${err}`);
      }
      fs.writeFileSync(dest + ".tmp", data);
      fs.renameSync(dest + ".tmp", dest);
      return;
    } catch (e) {
      lastErr = e;
      await new Promise(r => setTimeout(r, 250 * 2 ** attempt));
    }
  }
  die(`${entry.url}: ${lastErr}`);
}

async function main(manifestPath, outDir) {
  const entries = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
  fs.mkdirSync(outDir, {recursive: true});
  // Drop anything the lockfile no longer references, so the output only
  // depends on the manifest.
  const wanted = new Set(entries.map(e => e.name));
  for (const name of fs.readdirSync(outDir)) {
    if (!wanted.has(name)) {
      fs.rmSync(path.join(outDir, name), {recursive: true, force: true});
    }
  }
  let next = 0;
  const worker = async () => {
    while (next < entries.length) {
      await fetchOne(entries[next++], outDir);
    }
  };
  await Promise.all(Array.from({length: 16}, worker));
}

main(...process.argv.slice(2)).catch(e => die(e.stack || String(e)));
