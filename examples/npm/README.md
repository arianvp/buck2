# npm / bun example

Builds a JavaScript project straight from its lockfile with `npm_download`,
`npm_node_modules` and `npm_build`. There are no generated BUCK files for
packages. The same rules accept npm's `package-lock.json` (v2/v3) and bun's
text `bun.lock`. This example has both, generated from the same
`package.json`: `:dist` is built from the npm lockfile and `:bun_dist` from the
bun one.

```
app/
  BUCK                     # npm_download, npm_node_modules, npm_build
  package.json, package-lock.json, bun.lock
  index.html, src/         # Vite frontend (`npm run build` -> dist/)
  server.js                # Express server serving dist/ and /api/hello/:name
features/                  # lockfile features the app doesn't use (a test)
```

`features/` is a small project whose `npm run check` (`check.js`) fails if
one of these is broken: bins of a scoped package (`ni` from `@antfu/ni`)
must be on `PATH`, bundled dependencies (`jsonschema` and `semver` inside
`@aws-cdk/cloud-assembly-schema`) must load from their parent's tarball,
and `npm_build`'s `env` must reach the script. It also records the Unix
epoch as a date string, which is the same everywhere because `npm run` gets
`TZ=UTC`. `:check` uses `package-lock.json` and `:bun_check` uses
`bun.lock`.

## Running

The example uses the prelude from this repository. Buck2 does not follow
symlinks that leave the project, so copy it in first:

```sh
cd examples/npm
cp -r ../../prelude prelude
buck2 build //app:dist                 # vite build, offline
buck2 build //app:node_modules[prod]   # production node_modules for server.js
buck2 build //app:bun_dist             # same, from bun.lock
buck2 build //features:check //features:bun_check
```

With remote execution (see `examples/remote_execution/local`), everything
but the download runs on the workers:

```sh
cp ../remote_execution/local/re.buckconfig .buckconfig.local
buck2 build //app:dist //app:bun_dist //app:node_modules[prod] //features:check //features:bun_check
```

## How it works

0. For `bun.lock`, a small action first converts the lockfile into the
   package-lock.json shape: install paths, tarball URLs (bun's own, or
   registry.npmjs.org for packages bun lists without one), `dev`/`optional`
   worked out from the dependency graph, `os`/`cpu` and bins. Everything
   after this is shared with npm.
1. `npm_download` reads the lockfile and downloads every tarball for the
   target platform. Optional packages for other platforms are skipped. Each
   tarball is checked against its `integrity` hash and named after it. This
   is the only step with network access. It is incremental: unchanged
   tarballs are kept, and ones the lockfile no longer lists are deleted.
2. `npm_node_modules` extracts each tarball in its own action, whose inputs
   are just that tarball, node and the extract script, after checking its
   `integrity` hash again (the download is a
   local action, so its cached result comes from whichever machine ran it).
   It then assembles `node_modules` exactly as the lockfile lays it out,
   including nested packages and `.bin` links. Bundled dependencies
   (`bundleDependencies`) come with their parent's tarball. The default
   output includes dev dependencies; `[prod]` omits them. Both share the
   same extract actions.
3. `npm_build` runs `npm run build` in offline mode (`npm_config_offline`)
   against a scratch copy of the sources and `node_modules`. The script's
   environment variables are built from scratch, the same locally and
   remotely: `HOME` and `TMPDIR` are in the scratch directory, `TZ=UTC`,
   `LANG=C.UTF-8`, `CI=true`, `NODE_ENV=production`, no npmrc from outside
   the sources is read, and the rule's `env` is set over these. `PATH` is
   `node`, `npm` and `npx`, `node_modules/.bin`, `/usr/bin` and `/bin` (npm
   also adds its `node-gyp-bin` and the `.bin` of any `node_modules` above
   the working directory). `/usr/bin` and `/bin` are the machine's own,
   though: locally a script can run whatever the host has installed (`git`,
   `python3`, `curl`, ...), and `git rev-parse HEAD` then reads the checkout
   that contains `buck-out`. Neither is an input of the action, so the result
   doesn't change when they do. The worker only has what its image has, so
   such a script fails remotely (`git: not found` on the harness).

   npm, Node and build tools look for `node_modules`, `package.json` and
   config files (postcss, babel, ...) in every directory above the working
   directory. A local build runs in a new directory under `/tmp`, so the
   project's own (from an `npm install`, or a root `package.json` and
   postcss config) can't change its result: a build that needs one of them
   fails, or ignores it, locally the same way as remotely. Checked: a root
   postcss config, and packages and bins in a root `node_modules`. A
   `node_modules` or `package.json` above that `/tmp` directory fails the
   build. If `/tmp` is mounted noexec, the build runs in buck-out's scratch
   directory instead, where the project's files are visible again.

## Verified

- For this example and `features/`, `node_modules` is identical to
  `npm ci --ignore-scripts`: file list, contents, file modes and `.bin`
  links. `[prod]` matches `npm ci --omit=dev --ignore-scripts`, except that
  npm leaves empty scope directories behind for omitted scoped dev packages
  (`@esbuild`, `@napi-rs`, `@rollup` and `@types` here, `@antfu` in
  `features/`). `dist/` is identical to running `vite build` by hand.
  In general there is one more known difference: npm keeps a tarball's file
  modes other than 0644/0755 (e.g. 0744), while extract writes 0755 for any
  file with an executable bit and 0644 otherwise (buck2 and remote
  execution only keep the executable bit). Like npm, extract renames
  `.gitignore` files to `.npmignore` (checked against `npm install` of a
  crafted tarball).
- Tampering with an `integrity` hash fails the download with
  `integrity mismatch`. So does a tarball replaced in `buck-out` after the
  download, when it is extracted.
- In the build step, `npx` of a package that isn't installed fails with
  `ENOTCACHED`.
- No absolute paths appear in any action's command line, its environment
  (apart from buck2's own scratch variables), or the generated manifests.

What re-ran after each change:

| Change | What re-ran |
|---|---|
| Nothing | nothing |
| `src/greet.js` | `npm run build` only |
| Cosmetic lockfile edit (root `version`) | nothing |
| Bump `express` | download of 2 tarballs (express, path-to-regexp); 2 extracts; both assemblies; `npm run build` |

`npm run build` re-runs whenever `node_modules` changes, because the bundler
reads all of it. This is inherent, like the link step for Go.

bun (`bun.lock` targets):

- For this example, `node_modules` is identical to
  `bun install --ignore-scripts`: file list and contents. `[prod]` matches
  `bun install --production`. `dist/` matches. Not in general: see
  Limitations.
- The converter's `dev`/`optional` flags match npm's lockfile for all 133
  packages.
- Two bun conventions are deliberately not copied. bun makes `.bin` targets
  mode 777 (we use npm's 755). bun links `.bin/esbuild` straight to the
  native binary (we link esbuild's JS wrapper, like npm).
- A `bun.lock` edit that changes no packages re-runs only the conversion.
  Bumping express re-extracts only express and path-to-regexp.

## Remote execution

`npm_download` is the only local action (`local_only`: it needs the
network). The bun.lock conversion, every extract, both assemblies and
`npm run` run on the workers. Verified on the local NativeLink harness:

- All targets (`:dist`, `:bun_dist`, `:node_modules`, `:bun_node_modules`
  and their `[prod]`, and those of `features/`) build remotely, and every
  output is identical to a local build with remote execution off: file
  list, contents, modes (644 and 755), `.bin` symlinks and directory
  layout. The native binaries run on the worker: rollup's (for
  `vite build`) and esbuild's.
- Workers need `/bin/sh` (npm runs scripts with it), `/usr/bin/env` (package
  bins start with `#!/usr/bin/env node`), and glibc 2.28+ with libstdc++ for
  the official Node.js binary. The harness image
  (`python:3.12-slim-bookworm`) has all of them.
- Offline on the worker too: `npx` of a package that isn't installed fails
  with `ENOTCACHED`. The harness worker shares the host's network, though, so
  like a local build, a script that calls the network itself would reach it.

What each action sends to the worker (this example; `examples/typescript`
in brackets):

| Action | Inputs |
|---|---|
| extract (one per tarball) | `node` (125 MB), `npm_extract.cjs`, one tarball (84 tarballs, 9.0 MB in all [97, 19.6 MB]) |
| assemble, dev | `node`, `npm_tool.cjs`, a manifest, 84 extracted packages: 837 files, 24.6 MB [97: 1,511 files, 57.6 MB] |
| assemble, `[prod]` | same, 69 packages: 621 files, 2.2 MB [same] |
| `npm run` | `node`, `npm_tool.cjs`, the sources, `node_modules` as above, and npm's own package (1,964 files, 10.9 MB) |

The worker hard-links inputs from its CAS. `npm run` copies `node_modules`
into its scratch directory rather than hard-linking it: a script that writes
to a file in place would otherwise change the input, and on a worker that
hard-links from its CAS, the stored blob itself.

A second checkout (another absolute path, its own buck2 daemon and
buck-out) building the same targets against the same NativeLink gets every
action from the cache, including `npm_download`: 177/177, 2.7 s
(`examples/remote_execution/local/cross_checkout.sh`). Rechecked after
adding `features/`: a copy of this directory at another path, building all
`app/` and `features/` targets after a build here, got 190/190 actions from
the cache.

Two things to know about cache keys:

- Each action's script is one of its inputs, so editing it (a prelude
  update) re-runs that action everywhere. The scripts are split so that an
  edit invalidates as little as possible: `npm_fetch.cjs` is only used by
  `npm_download` (an edit re-downloads every tarball on machines without the
  previous output), `npm_extract.cjs` only by the extracts (an edit re-runs
  every extract once), and `npm_tool.cjs` by the bun.lock conversion, the
  assemblies and `npm run`. Adding a comment to `npm_tool.cjs` and building
  `//app:dist //app:bun_dist //app:node_modules[prod] //features:check
  //features:bun_check` re-ran 2 conversions, 5 assemblies and 4
  `npm run`s, and no extract or download.
- An action's key includes the paths of its inputs and outputs, and those
  include target labels: an extract's tarball path names the `npm_download`
  target, and its output path the `npm_node_modules` target. `:tarballs`
  and `:bun_tarballs` hold the same tarballs but extract them separately;
  projects only share extracts when both labels match (`examples/typescript`
  gets cache hits from this example because both use `//app:tarballs` and
  `//app:node_modules`).

## Limitations

- bun: text `bun.lock` only (not the old binary `bun.lockb`), registry
  packages only, no workspaces. Scripts are run with npm, so a `build` script
  that calls `bun` itself won't work yet. `bun patch` (`patchedDependencies`)
  fails the conversion. bun writes no tarball URL for packages from
  registry.npmjs.org; when it reads such an entry it uses the registry
  configured for the package's scope (`bunfig.toml` or `.npmrc`), while the
  conversion doesn't read those files and always uses registry.npmjs.org.
- Local builds see the host's `/usr/bin` and `/bin` (see "How it works");
  remote ones only what the worker image has.

- Install scripts (`postinstall` etc.) are not run, like
  `npm ci --ignore-scripts`. Packages that need them (e.g. node-gyp native
  addons) won't work yet.
- npm lockfile v2/v3 only. Workspaces, `link:` packages, and git or file
  dependencies are not supported.
- Offline mode stops npm from fetching, but local actions aren't sandboxed. A
  script that calls the network itself (e.g. `curl`) would still reach it.
- No `libc` filtering, because the lockfile doesn't record it. Both glibc and
  musl variants of packages like rollup's native binding are installed, which
  is what npm does too.
