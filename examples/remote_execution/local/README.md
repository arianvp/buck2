# Local remote-execution harness

Runs [NativeLink](https://github.com/TraceMachina/nativelink) (CAS, action
cache, scheduler and one worker) on this machine, so projects that use
`prelude//platforms:default` can be built with remote execution without a
cloud service.

The worker runs in a container that only has what its image provides. By
default that's `mirror.gcr.io/library/python:3.12-slim-bookworm` (Docker
Hub's `python:3.12-slim-bookworm`; remote actions must ask for exactly this
name, see `re.buckconfig`): `/bin/sh`, coreutils, tar/gzip and
python3 (the prelude's Go bootstrap runs `go_wrapper.py` with it). There's no
Go or Node.js, and the host's paths aren't mounted. An action that depends on
something it didn't declare (a host tool, a file outside its inputs, an
absolute path) fails here instead of silently working, as it would on a real
RE cluster.

## Usage

Needs Docker and a statically linked `nativelink` binary, e.g. from the
release image:

```sh
docker create --name nl ghcr.io/tracemachina/nativelink:v1.7.2
docker export nl | tar -x --wildcards 'nix/store/*/bin/nativelink'
docker rm nl
export NATIVELINK=$(echo $PWD/nix/store/*/bin/nativelink)
```

Then:

```sh
./run.sh start                      # cache kept in ~/.cache/buck2-re-harness
cd ../../go_mod
cp -r ../../prelude prelude         # the examples use this repository's prelude
cp ../remote_execution/local/re.buckconfig .buckconfig.local
buck2 build //app:server            # "Commands: N (cached: .., remote: .., local: ..)"
```

`run.sh stop` stops NativeLink; `run.sh wipe` also deletes the cache.

`re_stats.py` summarizes how the last command's actions ran (per category:
cache hits, remote, local; cache uploads of local actions):

```sh
buck2 log show | ../remote_execution/local/re_stats.py [--list]
```

`cross_checkout.sh` builds the same targets from this checkout and from a
second clone at another absolute path (its own daemon and buck-out) and
prints both checkouts' stats, to check that a second machine gets cache
hits:

```sh
./run.sh wipe && ./run.sh start
./cross_checkout.sh /srv/machine-b go_mod://app:server npm://app:dist,//app:node_modules
```

`tree_digest.py` prints one line per entry of output trees (content digest,
executable bit, symlink target), to compare two builds' outputs with `diff`.

NativeLink's worker-side `directory_cache` (with
`experimental_subtree_caching`) made things slower here: date-fns's tsc
action (21k-file node_modules input) took 22–25 s per edit with it, against
16–17 s without (the first run, which fills the cache, took 54 s). It is off.

Use NativeLink 1.x. With 0.7.10 the filesystem store raced on concurrent
uploads of the same blob ("Failed to rename file ... NotFound"), which made
actions fail after retries, and the server once hung completely while
serving a large `--materializations=all` download.

## How projects opt in

`prelude//platforms:default` runs everything locally, without a remote
cache, unless the root cell's `.buckconfig` enables remote execution, or
only the remote cache (`remote_cache_enabled`, `allow_cache_uploads`), see
`remote_execution_attrs()` in `prelude/platforms/defs.bzl`. `re.buckconfig` sets:

```ini
[buck2_re_client]            # where NativeLink listens
engine_address = grpc://127.0.0.1:50051
...
[buck2]
digest_algorithms = SHA256   # NativeLink doesn't support buck2's default SHA1

[execution_platform]
remote_enabled = true
remote_execution_properties = OSFamily=linux container-image=mirror.gcr.io/library/python:3.12-slim-bookworm
```

With `remote_enabled = true`, every action runs remotely except those marked
`local_only` (e.g. network downloads) or `prefer_local`. Failed remote actions
are not retried locally. The action cache is queried for all actions, and
local actions that set `allow_cache_upload = True` are uploaded to it, so
another checkout or machine using the same NativeLink gets cache hits for
them too.

The platform keeps the host's OS/CPU constraints, so toolchains `select()`
the same way with and without RE. The workers must therefore match the host's
OS and CPU (here: the host itself).
