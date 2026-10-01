# go_mod example

Builds a Go module straight from its `go.mod` / `go.sum` with
`go_mod_download` + `go_mod_binary`. There are no generated or vendored BUCK
files for third-party packages.

```
app/
  BUCK                 # go_mod_download + go_mod_binary
  go.mod, go.sum
  cmd/server/          # main package (chi, uuid, //go:embed)
  internal/greet/      # x/text, x/crypto (Go assembly), BurntSushi/toml
```

## Running

The example uses the prelude from this repository. Buck2 does not follow
symlinks that leave the project, so copy it in first:

```sh
cd examples/go_mod
cp -r ../../prelude prelude
buck2 run //app:server -- -greet "ada lovelace"
buck2 run //app:server   # serves http://localhost:8080/hello/{name}
```

## How it works

1. `go_mod_download` runs `go mod download` (locally, with network access) and
   outputs a `GOMODCACHE` directory. Its result is uploaded to the action
   cache for other machines to use, so it must only depend on `go.mod`,
   `go.sum` and the module proxy:
   - `go` runs with a cleared environment: only the proxy, CA certificate and
     `NETRC` variables are kept from buck2's, `HOME` and `GOPATH` point into
     the action's scratch directory, and the Go settings are explicit
     (`GOPROXY`, `GOSUMDB=off`, `GOFLAGS`, `GOTOOLCHAIN=local`, ...). A
     developer's `GOPRIVATE`, `GONOPROXY`, `GOFLAGS`, `GOPATH` or `~/.netrc`
     don't apply.
   - `go.sum` is the only source of checksums: the download fails if it lacks
     the checksum of any module or `go.mod` file that was fetched.
2. `go_mod_verify` checks that module cache against `go.sum`: `go mod verify`
   hashes the extracted modules and compares them with their `.ziphash`
   files (each holds the hash of a module's zip), and the `.ziphash` and
   `go.mod` files are compared with `go.sum`. So a cache whose module
   sources or `go.mod` files were modified (say, by a broken disk on the
   machine that uploaded it) fails here, before anything is compiled from
   it. Not covered: the `.info` metadata files (version timestamps, which
   end up in `go_list.json` but not in binaries), and a client that can
   write arbitrary action cache entries, which could forge this check's
   result as well.
3. `go_mod_binary` runs `go list -deps` offline against that cache
   (`GOPROXY=off`, cleared environment), then declares one compile action per
   package from a dynamic action. Each compile only sees the package's own
   files (projected out of the module cache, or taken from `srcs`) and the
   export data of its direct imports.

Because of that, changes stay incremental:

| Change | Recompiled |
|---|---|
| Function body in `internal/greet` | `greet` only (export data unchanged, so `main` is cut off) |
| `cmd/server/main.go` | `main` only |
| Bump `github.com/google/uuid` in go.mod/go.sum | `uuid` and `main` |
| Bump indirect `golang.org/x/sys` | `x/sys/cpu` and `x/crypto/blake2b` |
| Comment in `go.mod` | nothing (download, verification and `go list` re-run, outputs unchanged) |

With remote execution on, every action except `go_mod_download` runs on a
worker that only sees its declared inputs. Actions that run locally are not
sandboxed: the download, the verification and `go list` clear their
environment, but the compile and link actions run in buck2's environment,
as the prelude's other Go actions do.

## Remote execution

With remote execution on (see `examples/remote_execution/local`), only
`go_mod_download` runs locally (`local_only`: it needs the network); its
result is uploaded to the action cache. Everything else, including the Go
distribution's extraction and the prelude's bootstrap tools, runs on the
workers. Verified on the local NativeLink harness:

- `//app:server` builds with 497 remote actions and 1 local one, and the
  binary is byte-identical after `buck2 clean` and a forced re-execution
  (`--no-remote-cache`), and in a fully local build.
- `go list` output is independent of where the action ran: paths under the
  action's working directory are made relative (`go_wrapper --trim-cwd`),
  so `go_list.json` is byte-identical across remote executions and a local
  build.
- Forced downloads in a daemon started with `GONOPROXY`, `GOPRIVATE`,
  `GOFLAGS`, `GOFIPS140`, `GOPATH`, `GOSUMDB`, `GOPROXY=direct`, `GOINSECURE`
  and `GOVCS` set, and in a clean one, give identical module caches.
- The module cache drops the module zips after download: loading packages
  only needs the extracted modules and the `.mod` and `.ziphash` files.
  `go list` only gets `GOROOT/src` as input.
- A function-body edit in `internal/greet` executes 3 actions (`go list`,
  that package's compile, the link). Moving `github.com/google/uuid` to
  v1.5.0 recompiles `uuid` and `main`, and moving the indirect
  `golang.org/x/sys` to v0.35.0 recompiles `x/sys/cpu` and
  `x/crypto/blake2b` (each also re-runs the download, the verification,
  `go list` and the link). A tampered go.sum fails
  `go_mod_download` with a checksum mismatch, and the failure isn't cached.
  A module cache whose `uuid.go` was modified after the download fails
  `go_mod_verify`, which on a cache hit costs nothing.
- A second checkout at another path, with its own daemon and buck-out, gets
  all 498 actions from the cache, `go_mod_download` included (6 s;
  re-executing them all with `--no-remote-cache` takes 78 s).

Workers need `/bin/sh`, coreutils (`mkdir`), `tar` and `gzip` to unpack the Go
distribution (`http_archive`), and python3 for the prelude's Go bootstrap
(`go_wrapper.py`).

## Limitations

- Pure Go only: the target is built with `CGO_ENABLED=0`.
- The target must live next to `go.mod`. A `replace` directive that points
  inside the module root needs the replaced module's `go.mod` in
  `go_mod_download`'s `nested_go_mods`; ones that point outside it are not
  supported.
- Modules are only fetched from a module proxy (`goproxy`, by default
  `https://proxy.golang.org`), not from version control (`direct`). Private
  modules need a proxy that serves them; its credentials are read from the
  netrc file named by `NETRC` in buck2's environment (not from `~/.netrc`).
- `go.sum` must hold the checksum of everything `go mod download` fetches,
  as `go mod tidy` ensures. With `go 1.16` or older in `go.mod`, it can
  fetch modules that `go mod tidy` keeps no checksums for (the error says
  so): use `go 1.17` or later.
- One binary per target. Several binaries in the same module, also with
  different build tags, share one `go_mod_download` target and one download.
