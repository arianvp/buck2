# TypeScript example (client + server)

One TypeScript app built from `package-lock.json` with the prelude's npm
rules. Code in `src/shared/` is used by both the browser client and the Node
server. It uses TypeScript 7 (the native compiler, and what
`npm install typescript` installs today); `tsc_build` also runs TypeScript 5
(checked with 5.9), through Node.js.

```
app/
  src/client/main.ts     # browser code, bundled by Vite
  src/server/main.ts     # Express server
  src/shared/greet.ts    # used by both
  tsconfig.client.json   # DOM, bundler resolution, type-check only
  tsconfig.server.json   # Node, NodeNext modules, emits dist/server
  vite.config.ts
```

| Target | Rule | Output |
|---|---|---|
| `:client_types` | `tsc_build` (`tsconfig.client.json`, `noEmit`) | stamp; fails on type errors |
| `:server` | `tsc_build` (`tsconfig.server.json`, `out = "dist/server"`) | compiled JS; run next to `:node_modules[prod]` |
| `:client` | `npm_build` (`vite build`, `checks = [":client_types"]`) | `dist/client` (bundled JS + index.html) |
| `:server_bundle` | `npm_build` (`esbuild --bundle`, `checks = [":server"]`) | `bundle/server.cjs` (one file, no node_modules needed) |

## Running

The example uses the prelude from this repository. Buck2 does not follow
symlinks that leave the project, so copy it in first:

```sh
cd examples/typescript
cp -r ../../prelude prelude
buck2 build //app:client //app:server //app:server_bundle //app:node_modules[prod]
```

To run the compiled server, put `:server`'s output, `:node_modules[prod]` and
`:client`'s output (as `client/`) in one directory with a `package.json`
containing `{"type": "module"}`. Then run
`STATIC_DIR=client node server/main.js`. The bundled server only needs
`server.cjs` and `client/`: `STATIC_DIR=client node server.cjs`.

## Container image

`//app:image` is an OCI image of the bundled server:
`gcr.io/distroless/cc-debian12` (glibc and libstdc++, which the official
Node.js binary needs; no shell), pinned by digest with `oci_pull`, then the
toolchain's own `node` (`toolchains//:node_bin`) as one layer and the app
(`server.cjs` and the client) as the last one.

```sh
docker load -i $(buck2 build //app:image[tarball] --show-output | cut -d' ' -f2)
docker run --rm -p 3000:3000 buck2-typescript-server
```

Verified on the local NativeLink harness: the container serves the client
and `/api/hello/:name` as `nonroot`, from Docker 29 (containerd image store)
and Docker 28.5.2 (classic store); the image digest is the same on RE, in a
fully local build after `buck2 clean` (109 local actions, remote cache off;
local Python 3.11 with zlib 1.3, the workers' Python 3.12 with zlib 1.2.13),
and from a second checkout (109 of 109 actions from the cache). A change to
`greet()` in `src/shared/` re-runs the type checks, the bundles, the app
layer and the manifest, not the `node` layer; adding a layer in front only
runs that layer and the manifest.

The image doesn't build on macOS yet: `oci_image` configures its
dependencies for Linux, and the npm rules use one `node` (the target
platform's) both to run the type checks and bundlers and to choose
platform-specific packages, so a Mac would have to run the Linux `node`
(inferred from the configuration, not tried on a Mac).

## How `tsc_build` works

Each `tsc_build` target is one action that runs the project's own
`tsc -p <tsconfig>` on a scratch copy of its `srcs`, from scratch, with
`node_modules` as an input. Nothing is kept between builds, so the result is
cached by its inputs and shared through remote execution: any change to the
target's sources or `node_modules` re-runs the whole check, and anything else
is a cache hit. TypeScript 7's native binary is run directly (no Node.js
startup) when its platform package is installed; older versions run through
`node typescript/bin/tsc`.

tsc looks for `node_modules` (TypeScript 5 and older also for every
`node_modules/@types`) and for `package.json`, which decides a file's module
format, in every directory above its working directory. Locally that would be
the buck2 project itself, so a local check runs in a new directory under
`/tmp`, with `node_modules` hard-linked in (copied if `/tmp` is another
filesystem); a `node_modules` or `package.json` above that directory fails
the check. If `/tmp` is mounted noexec, the check runs in buck-out's scratch
directory instead, where the project's files are visible again. Checked: an import only a root
`node_modules` could satisfy fails locally like remotely (`TS2307`, with
TypeScript 5.9 and 7.0), a root `package.json` with `"type": "module"` no
longer turns a local CommonJS emit into ESM, and a `.tsbuildinfo` in the
output is byte-identical locally and remotely.

## Verified

On the local NativeLink harness (`examples/remote_execution/local`), with
remote execution on:

- All four targets build remotely; only the tarball download runs locally.
  Every output is byte-identical to a local build with remote execution off,
  and after `buck2 clean` plus a forced re-execution (`--no-remote-cache`).
  `dist/server` is byte-identical to plain `npm ci && tsc -p
  tsconfig.server.json`.
- A type error in `src/shared/greet.ts` fails `:server` remotely with `tsc`'s
  message (`error TS2322`). Restoring the file gives cache hits for all four
  actions.
- The bundled server, built remotely, serves the client and
  `/api/hello/:name` from a directory holding only `server.cjs` and
  `client/`.

## Why one action per tsconfig

This is the granularity TypeScript itself builds at (`tsc -b` builds one
project per tsconfig, and large codebases split along project references),
and the one Bazel's `rules_ts` uses. A finer split was investigated and not
adopted: check each group of files ("unit") in its own action against the
`.d.ts` summaries of the units it imports, so that an edit only re-checks
its own unit. The numbers below are labelled by how they were obtained.

Measured, `tsc_build` on the local NativeLink harness:

| Project | Compiler | Plain `tsc` on the host | Build after an edit (tsc action) | No change |
|---|---|---|---|---|
| synthetic, 3,000 files | TS 7.0.2 | 4.5–5.0 s | 11.6–12.6 s (9.8–10.8 s) | 0.05 s |
| date-fns 4.1.0 | TS 5.4.5 (its own) | 8.4–9.0 s | 14.9–17.9 s (13.5–16.9 s) | 0.05 s |

Every edit re-checks the whole project, and remote execution adds a few
seconds for the action's thousands of input and output files. A second
checkout gets every result from the cache.

Measured outside buck2, per-unit checking with TypeScript 7:

- It is not equivalent to one `tsc` run. Each unit only sees other files'
  `.d.ts` summaries, which drop information (private members lose their
  types) and globals that a file elsewhere in the program brings in (e.g.
  `@types/node`). On vscode it reported all 385 errors of the full check
  plus about 65 that `tsc` doesn't (after fixes that had removed 4,746
  more); on effect, 7 extra errors. 16% of date-fns's emitted `.d.ts` files
  differ textually from `tsc`'s.
- Adding one import changes the membership of about 16 of 106 units on
  vscode, so unchanged files miss the cache.

Modelled (per-unit TypeScript times measured, scheduled onto 4–64 workers
with an assumed 0.2–1 s of remote-execution overhead per action; not run
through buck2): per-unit checking never beat one whole-project action on a
clean build, and only shortened edits once the whole-project check took more
than about 2–3 s.
