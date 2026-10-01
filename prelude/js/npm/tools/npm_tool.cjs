/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

// Helper for the prelude's npm rules (prelude/js/npm/npm.bzl). Uses only
// Node.js built-ins so it can run with nothing but a Node distribution.
// Downloading and extracting packages are in npm_fetch.cjs and
// npm_extract.cjs, so that editing this file doesn't change those actions'
// keys.
//
//   assemble <manifest.json> <out_dir>
//       Lay out a node_modules tree from extracted packages and create
//       .bin links.
//   bun-lock <bun.lock> <out.json>
//       Convert a bun.lock into the package-lock.json v3 subset npm.bzl reads.
//   run --node <node> --npm <npm package dir> --srcs <manifest.json>
//       --node-modules <dir> --script <name> --out-dir <dir> --output <dir>
//       [--env <env.json>]
//       Run `npm run <script>` offline in a scratch copy of the sources and
//       copy <out-dir> to <output>.

"use strict";

const childProcess = require("child_process");
const fs = require("fs");
const path = require("path");

function die(msg) {
  process.stderr.write(`npm_tool: ${msg}\n`);
  process.exit(1);
}

// ---------------------------------------------------------------------------
// assemble

function binName(pkgPath, name) {
  // A string "bin" is named after the package ("@scope/name" -> "name").
  return name !== "" ? name : path.basename(pkgPath);
}

// The node_modules directory a package is installed in, relative to the
// output: "" for "a" and "@scope/a", "a/node_modules" for
// "a/node_modules/b" and "a/node_modules/@scope/b".
function parentModulesDir(pkgPath) {
  const parts = pkgPath.split("/");
  const scoped = parts.length >= 2 && parts[parts.length - 2].startsWith("@");
  return parts.slice(0, scoped ? -2 : -1).join("/");
}

function cmdAssemble(manifestPath, outDir) {
  const {packages} = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
  fs.mkdirSync(outDir, {recursive: true});
  // Parents before nested packages (node_modules/a before node_modules/a/node_modules/b),
  // then by code point (not localeCompare, which depends on the locale):
  // this order decides which package's bin wins when two have the same name.
  const depth = p => p.path.split("/").length;
  packages.sort((a, b) => depth(a) - depth(b) || (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
  for (const pkg of packages) {
    // Bundled dependencies have no directory of their own: they came with
    // their parent's.
    if (pkg.dir !== null) {
      fs.cpSync(pkg.dir, path.join(outDir, pkg.path), {recursive: true});
    }
  }
  for (const pkg of packages) {
    const bins = typeof pkg.bin === "string" ? {"": pkg.bin} : {...pkg.bin};
    if (pkg.bin_dir) {
      // Like bun: every file directly in the directory, named after it.
      const pkgDir = path.join(outDir, pkg.path);
      const dir = path.join(pkgDir, pkg.bin_dir);
      if (!path.relative(pkgDir, dir).startsWith("..") && fs.existsSync(dir)) {
        for (const entry of fs.readdirSync(dir, {withFileTypes: true})) {
          if (entry.isFile() || entry.isSymbolicLink()) {
            bins[entry.name] = path.join(pkg.bin_dir, entry.name);
          }
        }
      }
    }
    const binDir = path.join(outDir, parentModulesDir(pkg.path), ".bin");
    for (const [name, rel] of Object.entries(bins)) {
      const target = path.join(outDir, pkg.path, rel);
      if (!fs.existsSync(target)) {
        continue;
      }
      fs.mkdirSync(binDir, {recursive: true});
      const link = path.join(binDir, binName(pkg.path, name));
      fs.rmSync(link, {force: true});
      fs.symlinkSync(path.relative(binDir, target), link);
      fs.chmodSync(target, 0o755);
    }
  }
}

// ---------------------------------------------------------------------------
// run

// Copy (not hard-link) a file so that tools writing to it only touch the
// scratch copy. A hard link shares the inode with the input artifact, and
// with RE workers that hard-link inputs from their CAS (like NativeLink)
// with the CAS blob itself, so an in-place write would corrupt content
// stored under its old digest. FICLONE makes copies cheap on filesystems
// with reflinks (btrfs, XFS) and falls back to a plain copy. The copy is
// writable by its owner: inputs are read-only on RE workers (0444/0555,
// 0644/0755 locally), and a worker that doesn't run as root couldn't write
// to it otherwise.
function copyFile(src, dest) {
  fs.copyFileSync(src, dest, fs.constants.COPYFILE_FICLONE);
  const mode = fs.statSync(dest).mode & 0o7777;
  if ((mode & 0o200) === 0) {
    fs.chmodSync(dest, mode | 0o200);
  }
}

function copyTree(src, dest) {
  fs.mkdirSync(dest, {recursive: true});
  for (const entry of fs.readdirSync(src, {withFileTypes: true})) {
    const s = path.join(src, entry.name);
    const d = path.join(dest, entry.name);
    if (entry.isDirectory()) {
      copyTree(s, d);
    } else if (entry.isSymbolicLink()) {
      fs.symlinkSync(fs.readlinkSync(s), d);
    } else {
      copyFile(s, d);
    }
  }
}

function parseFlags(argv) {
  const flags = {};
  for (let i = 0; i < argv.length; i += 2) {
    if (!argv[i].startsWith("--")) {
      die(`unexpected argument ${argv[i]}`);
    }
    flags[argv[i].slice(2)] = argv[i + 1];
  }
  return flags;
}

// Files that tools find by walking up from their working directory, and so
// would be undeclared inputs if they sat above the scratch directory.
const FOUND_BY_WALKING_UP = ["node_modules", "package.json"];

// The first of FOUND_BY_WALKING_UP in `dir` or one of its parents, or null.
function foundAbove(dir) {
  for (let d = dir; ; d = path.dirname(d)) {
    for (const name of FOUND_BY_WALKING_UP) {
      const candidate = path.join(d, name);
      if (fs.lstatSync(candidate, {throwIfNoEntry: false})) {
        return candidate;
      }
    }
    if (path.dirname(d) === d) {
      return null;
    }
  }
}

// Whether `dir` is on a filesystem mounted noexec (Linux only; elsewhere
// assume it isn't).
function isNoexec(dir) {
  let mounts;
  try {
    mounts = fs.readFileSync("/proc/self/mounts", "utf8");
  } catch (e) {
    return false;
  }
  let best = null;
  for (const line of mounts.split("\n")) {
    const [, mountPoint, , options] = line.split(" ");
    if (mountPoint && (dir === mountPoint || dir.startsWith(mountPoint.replace(/\/?$/, "/")))) {
      if (best === null || mountPoint.length > best.mountPoint.length) {
        best = {mountPoint, options};
      }
    }
  }
  return best !== null && best.options.split(",").includes("noexec");
}

// A scratch directory with nothing above it that tools could pick up.
// Tools look for node_modules, package.json and their own configs (postcss,
// babel, ...) in every parent of their working directory. On an RE worker,
// the working directory is the action's input root: everything above
// buck2's scratch directory is a declared input, so it is used as is (and
// on the same filesystem as the inputs, so links are cheap). Locally, the
// working directory is the buck2 project (it has a .buckconfig), typically
// a JS repo with its own package.json, node_modules and configs that a
// remote build doesn't see; so work in a new directory under /tmp (removed
// on exit) instead. If /tmp is mounted noexec, native binaries copied there
// couldn't run: fall back to the scratch directory and accept what is above.
function isolatedScratch(prefix) {
  const scratch = process.env.BUCK_SCRATCH_PATH ? path.resolve(process.env.BUCK_SCRATCH_PATH) : null;
  if (scratch !== null && !fs.existsSync(".buckconfig") && foundAbove(scratch) === null) {
    return scratch;
  }
  if (scratch !== null && isNoexec("/tmp")) {
    return scratch;
  }
  const tmp = fs.mkdtempSync(path.join("/tmp", prefix));
  process.on("exit", () => fs.rmSync(tmp, {recursive: true, force: true}));
  const found = foundAbove(tmp);
  if (found !== null) {
    die(`${found} would be visible to the build, which only gets its declared inputs; remove it`);
  }
  return tmp;
}

function cmdRun(argv) {
  const f = parseFlags(argv);
  const node = path.resolve(f.node);
  const npmDir = path.resolve(f.npm);
  const srcs = JSON.parse(fs.readFileSync(f.srcs, "utf8"));
  const output = path.resolve(f.output);

  const scratch = isolatedScratch("npm-run-");
  const work = path.join(scratch, "work");
  fs.rmSync(work, {recursive: true, force: true});
  for (const [rel, src] of Object.entries(srcs)) {
    fs.mkdirSync(path.dirname(path.join(work, rel)), {recursive: true});
    copyFile(src, path.join(work, rel));
  }
  copyTree(path.resolve(f["node-modules"]), path.join(work, "node_modules"));

  // Scripts often call `node`, `npm` or `npx`, and npm doesn't put them on
  // PATH. Only `node` and npm's package are inputs (not the distribution's
  // bin/), so link them here; npm-cli.js and npx-cli.js are
  // `#!/usr/bin/env node` scripts.
  const bin = path.join(scratch, "bin");
  const home = path.join(scratch, "home");
  const tmp = path.join(scratch, "tmp");
  for (const dir of [bin, home, tmp]) {
    fs.mkdirSync(dir, {recursive: true});
  }
  fs.symlinkSync(node, path.join(bin, "node"));
  fs.symlinkSync(path.join(npmDir, "bin", "npm-cli.js"), path.join(bin, "npm"));
  fs.symlinkSync(path.join(npmDir, "bin", "npx-cli.js"), path.join(bin, "npx"));

  // A fresh environment, not the inherited one: remote actions only get what
  // buck2 declares, but local ones inherit the buck2 daemon's environment
  // (NODE_OPTIONS, NODE_PATH, npm_config_*, proxies, TZ, LANG), so local and
  // remote builds could differ. npm runs scripts with /bin/sh, and package
  // bins start with `#!/usr/bin/env node`. /usr/bin and /bin are the
  // machine's own: locally they have whatever the host has installed.
  const env = {
    PATH: [bin, path.join(work, "node_modules", ".bin"), "/usr/bin", "/bin"].join(path.delimiter),
    HOME: home,
    TMPDIR: tmp,
    // Without TZ, the time zone comes from the machine's /etc/localtime.
    TZ: "UTC",
    LANG: "C.UTF-8",
    CI: "true",
    NODE_ENV: "production",
    npm_config_cache: path.join(scratch, "npm-cache"),
    // Neither file exists, so no npmrc is read from outside the inputs (the
    // default global one is <node prefix>/etc/npmrc). A project .npmrc in
    // srcs still applies.
    npm_config_globalconfig: path.join(scratch, "npmrc"),
    npm_config_userconfig: path.join(home, ".npmrc"),
    npm_config_offline: "true",
    npm_config_audit: "false",
    npm_config_fund: "false",
    npm_config_update_notifier: "false",
    // npm_build's `env`: declared, so part of the action key.
    ...(f.env ? JSON.parse(fs.readFileSync(f.env, "utf8")) : {}),
  };
  const npmCli = path.join(npmDir, "bin", "npm-cli.js");
  const res = childProcess.spawnSync(node, [npmCli, "run", f.script], {cwd: work, env, stdio: ["ignore", process.stderr, "inherit"]});
  if (res.status !== 0) {
    die(`npm run ${f.script} failed (${res.error || "exit " + res.status})`);
  }

  const built = path.join(work, f["out-dir"]);
  if (!fs.statSync(built, {throwIfNoEntry: false})?.isDirectory()) {
    die(`npm run ${f.script} did not produce the directory "${f["out-dir"]}"`);
  }
  copyOutput(built, output, [work, fs.realpathSync(work)]);
}

// Copy the script's output. Relative symlinks are kept as they are. An
// absolute one into the work directory (e.g. `ln -s "$PWD/dist/x"`) is made
// relative: its target only exists during this action, local builds would
// keep a dangling link, and remote execution may reject the output
// (NativeLink 1.7 does). Any other absolute link fails the build, locally
// and remotely alike, since it points into the machine that ran the build.
function copyOutput(src, dest, workDirs) {
  fs.mkdirSync(dest, {recursive: true});
  for (const entry of fs.readdirSync(src, {withFileTypes: true})) {
    const s = path.join(src, entry.name);
    const d = path.join(dest, entry.name);
    if (entry.isDirectory()) {
      copyOutput(s, d, workDirs);
    } else if (entry.isSymbolicLink()) {
      let target = fs.readlinkSync(s);
      if (path.isAbsolute(target)) {
        const abs = path.normalize(target);
        const work = workDirs.find(w => abs === w || abs.startsWith(w + path.sep));
        if (work === undefined) {
          die(`${path.relative(workDirs[0], s)} links to ${target}, outside the build directory; use a relative link`);
        }
        // `s` is under workDirs[0].
        target = path.relative(path.dirname(s), path.join(workDirs[0], path.relative(work, abs))) || ".";
      }
      fs.symlinkSync(target, d);
    } else if (entry.isFile()) {
      fs.copyFileSync(s, d);
    } else {
      die(`${path.relative(workDirs[0], s)} is not a file, directory or symlink`);
    }
  }
}

// ---------------------------------------------------------------------------
// bun-lock

function parseJsonWithTrailingCommas(text) {
  // bun.lock is JSON plus trailing commas. Drop a comma when the next
  // non-whitespace character closes an object/array, outside of strings.
  let out = "";
  let inString = false;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (inString) {
      out += c;
      if (c === "\\") {
        out += text[++i];
      } else if (c === '"') {
        inString = false;
      }
    } else if (c === '"') {
      inString = true;
      out += c;
    } else if (c === ",") {
      const rest = text.slice(i + 1).match(/^\s*(.)/s);
      if (!rest || (rest[1] !== "}" && rest[1] !== "]")) {
        out += c;
      }
    } else {
      out += c;
    }
  }
  return JSON.parse(out);
}

// Split a bun.lock key ("send/ms", "a/@scope/b") into package names.
function bunKeyNames(key) {
  const parts = key.split("/");
  const names = [];
  for (let i = 0; i < parts.length; i++) {
    names.push(parts[i].startsWith("@") ? parts[i] + "/" + parts[++i] : parts[i]);
  }
  return names;
}

function asList(v) {
  return v === undefined ? undefined : Array.isArray(v) ? v : [v];
}

// Convert bun.lock (text lockfile, bun >= 1.2) into the subset of the
// package-lock.json v3 format that npm.bzl reads.
function cmdBunLock(lockPath, outPath) {
  const lock = parseJsonWithTrailingCommas(fs.readFileSync(lockPath, "utf8"));
  const workspaces = lock.workspaces || {};
  if (Object.keys(workspaces).length !== 1) {
    die("bun.lock: workspaces are not supported yet");
  }
  const root = workspaces[""];
  if (Object.keys(lock.patchedDependencies || {}).length > 0) {
    die("bun.lock: patchedDependencies (`bun patch`) are not supported yet");
  }

  const entries = {};
  for (const [key, value] of Object.entries(lock.packages || {})) {
    const [spec, url, meta, integrity] = value;
    if (value.length !== 4 || typeof url !== "string" || typeof integrity !== "string") {
      die(`bun.lock: "${key}" is not an npm registry package (git, file and workspace packages are not supported)`);
    }
    const at = spec.lastIndexOf("@");
    const name = spec.slice(0, at);
    const version = spec.slice(at + 1);
    entries[key] = {
      names: bunKeyNames(key),
      name,
      version,
      meta: meta || {},
      // bun writes the tarball URL, or "" for one under registry.npmjs.org.
      // Reading "", bun uses the registry configured for the package's scope
      // (bunfig.toml, .npmrc); those files aren't read here, so it is
      // always registry.npmjs.org.
      resolved: url !== "" ? url : `https://registry.npmjs.org/${name}/-/${name.split("/").pop()}-${version}.tgz`,
      integrity,
    };
  }

  // Resolve a dependency like Node does: nearest enclosing node_modules first.
  function resolve(fromNames, dep) {
    for (let n = fromNames.length; n >= 0; n--) {
      const key = [...fromNames.slice(0, n), dep].join("/");
      if (entries[key]) {
        return key;
      }
    }
    return null;
  }

  function depsOf(meta, includeOptional) {
    const optionalPeers = new Set(meta.optionalPeers || []);
    const peers = Object.keys(meta.peerDependencies || {}).filter(p => !optionalPeers.has(p));
    return [
      ...Object.keys(meta.dependencies || {}).map(d => [d, false]),
      ...peers.map(d => [d, false]),
      ...(includeOptional ? Object.keys(meta.optionalDependencies || {}).map(d => [d, true]) : []),
    ];
  }

  // Walk from the root: `seen` gets everything reachable, `required` only what
  // is reachable without going through an optionalDependencies edge.
  function walk(rootDeps) {
    const seen = new Set();
    const required = new Set();
    const queue = rootDeps.map(([d, optional]) => [[], d, !optional]);
    while (queue.length > 0) {
      const [fromNames, dep, isRequired] = queue.shift();
      const key = resolve(fromNames, dep);
      if (key === null || (seen.has(key) && (!isRequired || required.has(key)))) {
        continue;
      }
      seen.add(key);
      if (isRequired) {
        required.add(key);
      }
      for (const [d, optional] of depsOf(entries[key].meta, true)) {
        queue.push([entries[key].names, d, isRequired && !optional]);
      }
    }
    return {seen, required};
  }

  const prod = walk(depsOf(root, true));
  const all = walk([...depsOf(root, true), ...Object.keys(root.devDependencies || {}).map(d => [d, false])]);

  const packages = {"": {name: root.name}};
  for (const [key, e] of Object.entries(entries)) {
    const entry = {
      version: e.version,
      resolved: e.resolved,
      integrity: e.integrity,
    };
    if (!prod.seen.has(key)) {
      entry.dev = true;
    }
    if (!all.required.has(key)) {
      entry.optional = true;
    }
    // bun writes "none" for platforms it doesn't know; like any unknown
    // value it never matches, so those optional packages are skipped.
    if (e.meta.os) {
      entry.os = asList(e.meta.os);
    }
    if (e.meta.cpu) {
      entry.cpu = asList(e.meta.cpu);
    }
    if (e.meta.bin) {
      entry.bin = e.meta.bin;
    }
    // package.json's `directories.bin`, which bun keeps as is (npm's
    // lockfile lists the files instead). Not a package-lock.json field;
    // assemble links the files in it.
    if (e.meta.binDir) {
      entry.binDir = e.meta.binDir;
    }
    packages["node_modules/" + e.names.join("/node_modules/")] = entry;
  }
  fs.writeFileSync(outPath, JSON.stringify({lockfileVersion: 3, packages}, null, 2) + "\n");
}

// ---------------------------------------------------------------------------

async function main() {
  const [cmd, ...args] = process.argv.slice(2);
  switch (cmd) {
    case "assemble":
      return cmdAssemble(args[0], args[1]);
    case "run":
      return cmdRun(args);
    case "bun-lock":
      return cmdBunLock(args[0], args[1]);
    default:
      die(`unknown command ${cmd}`);
  }
}

main().catch(e => die(e.stack || String(e)));
