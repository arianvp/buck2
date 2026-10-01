/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

// Helper for the prelude's TypeScript rules (prelude/js/npm/npm.bzl). Uses
// only Node.js built-ins. Kept apart from npm_tool.cjs so that changing it
// doesn't change the inputs (and cache keys) of the npm package actions.
//
//   tsc --node <node> --srcs <manifest.json> --node-modules <dir>
//       --tsconfig <path> [--out-dir <dir>] --output <path>
//       Run the project's `tsc -p <tsconfig>` on a scratch copy of the
//       sources; copy <out-dir> (or write a stamp file) to <output>.

"use strict";

const childProcess = require("child_process");
const fs = require("fs");
const path = require("path");

function die(msg) {
  process.stderr.write(`ts_tool: ${msg}\n`);
  process.exit(1);
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

function writeOutFile(file, data) {
  fs.mkdirSync(path.dirname(file), {recursive: true});
  fs.writeFileSync(file, data);
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

// Hard-link a tree (falling back to copying across filesystems). Only for
// trees nothing writes to: tsc only reads node_modules.
function linkTree(src, dest) {
  fs.mkdirSync(dest, {recursive: true});
  for (const entry of fs.readdirSync(src, {withFileTypes: true})) {
    const s = path.join(src, entry.name);
    const d = path.join(dest, entry.name);
    if (entry.isDirectory()) {
      linkTree(s, d);
    } else if (entry.isSymbolicLink()) {
      fs.symlinkSync(fs.readlinkSync(s), d);
    } else {
      try {
        fs.linkSync(s, d);
      } catch (e) {
        fs.copyFileSync(s, d, fs.constants.COPYFILE_FICLONE);
      }
    }
  }
}

// The compiler in node_modules: TypeScript 7's native binary when its
// platform package is installed (run directly: no node startup, and it reads
// its lib*.d.ts from next to itself), otherwise `node typescript/bin/tsc`.
function findTsc(node, nodeModules) {
  const pkg = path.join(nodeModules, "typescript");
  if (!fs.existsSync(path.join(pkg, "package.json"))) {
    die(`node_modules has no "typescript" package; add it to devDependencies`);
  }
  // Like typescript/lib/getExePath.js: @typescript/typescript-<platform>-<arch>,
  // nested under typescript/ or hoisted.
  const platformPkg = path.join("@typescript", `typescript-${process.platform}-${process.arch}`);
  for (const dir of [path.join(pkg, "node_modules"), nodeModules]) {
    const exe = path.join(dir, platformPkg, "lib", "tsc");
    if (fs.existsSync(exe)) {
      return [exe];
    }
  }
  return [node, path.join(pkg, "bin", "tsc")];
}

// Run the project's own `tsc -p <tsconfig>` (any version) on a scratch copy
// of the sources, from scratch every time: the action is hermetic, so its
// result can be cached and shared (e.g. through remote execution).
function cmdTsc(argv) {
  const f = parseFlags(argv);
  const node = path.resolve(f.node);
  const srcs = JSON.parse(fs.readFileSync(f.srcs, "utf8"));
  const nodeModules = path.resolve(f["node-modules"]);
  const output = path.resolve(f.output);

  const scratch = isolatedScratch("tsc-");
  const work = path.join(scratch, "work");
  fs.rmSync(work, {recursive: true, force: true});
  for (const [rel, src] of Object.entries(srcs)) {
    writeOutFile(path.join(work, rel), fs.readFileSync(src));
  }
  // A real tree, not a link to the input: tsc resolves imports from a
  // file's real path, and records paths (e.g. in .tsbuildinfo) relative to
  // it, so with a link those would point back into buck-out.
  const workNodeModules = path.join(work, "node_modules");
  linkTree(nodeModules, workNodeModules);

  const [exe, ...args] = findTsc(node, workNodeModules);
  const res = childProcess.spawnSync(
    exe,
    [...args, "-p", f.tsconfig, "--pretty", "false"],
    {
      cwd: work,
      // A fresh environment, the same locally and remotely.
      env: {PATH: [path.dirname(node), "/usr/bin", "/bin"].join(path.delimiter), HOME: scratch, TMPDIR: scratch},
      stdio: ["ignore", process.stderr, "inherit"],
    },
  );
  if (res.status !== 0) {
    die(`tsc -p ${f.tsconfig} failed (${res.error || "exit " + res.status})`);
  }

  if (f["out-dir"]) {
    const emitted = path.join(work, f["out-dir"]);
    if (!fs.existsSync(emitted)) {
      die(`tsc -p ${f.tsconfig} did not produce "${f["out-dir"]}"`);
    }
    fs.cpSync(emitted, output, {recursive: true, verbatimSymlinks: true});
  } else {
    writeOutFile(output, "ok\n");
  }
}

// ---------------------------------------------------------------------------

function main() {
  const [cmd, ...args] = process.argv.slice(2);
  switch (cmd) {
    case "tsc":
      return cmdTsc(args);
    default:
      die(`unknown command ${cmd}`);
  }
}

main();
