---
id: remote_execution
title: Remote Execution
---

Buck2 can use services that expose
[Bazel's remote execution API](https://github.com/bazelbuild/remote-apis) in
order to run actions remotely.

Buck2 projects have been successfully tested for remote execution against
[EngFlow](https://www.engflow.com/),
[BuildBarn](https://github.com/buildbarn/bb-remote-execution) and
[BuildBuddy](https://www.buildbuddy.io). Sample project configurations for those
providers are available under
[examples/remote_execution](https://github.com/facebook/buck2/tree/main/examples/remote_execution).

## RE configuration in `.buckconfig`

Configuration for remote execution can be found under `[buck2_re_client]` in
`.buckconfig`.

Keys supported include:

- `engine_address` - address to your RE's engine.
- `action_cache_address` - address to your action cache endpoint.
- `cas_address` - address to your content-addressable storage (CAS) endpoint.
- `tls_ca_certs` - path to a CA certificates bundle. This must be PEM-encoded.
  If none is set, a default bundle will be used. This path contains environment
  variables using shell interpolation syntax (i.e. $VAR). They will be
  substituted before reading the file.
- `tls_client_cert` - path to a client certificate (and intermediate chain), as
  well as its associated private key. This must be PEM-encoded. This path can
  contain environment variables using shell interpolation syntax (i.e. $VAR).
  They will be substituted before reading the file.
- `http_headers` - HTTP headers to inject in all requests to RE. This is a
  comma-separated list of `Header: Value` pairs. Minimal validation of those
  headers is done here. This can contain environment variables using shell
  interpolation syntax ($VAR). They will be substituted before reading the file.
- `instance_name` - an instance name to pass on execution, action cache, and CAS
  requests.

Buck2 uses `SHA256` for all its hashing by default. If your RE engine requires
something else, this can be configured in `.buckconfig` as follows:

```ini
[buck2]
# Accepts BLAKE3, SHA1, or SHA256
digest_algorithms = BLAKE3
```

## Sharing downloaded blobs between daemons

Every buck2 daemon keeps its outputs under `buck-out/<isolation-dir>`, so two
daemons with different [isolation directories](../../concepts/isolation_dir.md),
or two checkouts of the same repository, each download and store their own copy
of every blob they need. `buck2-casd`, a machine-local CAS daemon that ships
with buck2, removes that duplication. It is the open-source counterpart of the
shared CAS daemon buck2 uses at Meta and takes the same configuration keys.

The daemon speaks the remote execution API's CAS and ByteStream services over a
Unix socket inside its directory, and never listens anywhere off the machine.
Buck2 sends all CAS traffic to it; it passes misses and uploads through to the
real CAS and keeps every blob it has seen as a raw, read-only file in a
directory it alone owns. Given that directory, buck2 materializes outputs by cloning those
files instead of receiving bytes over gRPC. On btrfs, XFS and APFS the clone is
a reflink, so the data exists once on disk however many daemons use it. One
store, one downloader, one eviction policy, any number of isolation dirs.

Configure it in `.buckconfig` next to the other remote execution settings:

```ini
[buck2_re_client]
engine_address = grpc://re.example.com:443
action_cache_address = grpc://re.example.com:443
cas_address = grpc://cas.example.com:443
tls = true
cas_shared_cache = /var/cache/buck2-casd
cas_shared_cache_max_size_bytes = 53687091200
```

Buck2 starts the daemon itself the first time nothing answers at
`/var/cache/buck2-casd/buck2-casd.sock`, using the `buck2-casd` binary next to
the `buck2` executable (or on `PATH`), and hands it the directory, the digest
function from `[buck2] digest_algorithms`, the size cap, and the `cas_address`,
TLS and instance name settings above as its upstream. HTTP headers are passed
through the environment, not the command line. The daemon runs in its own
session and outlives the buck2 daemon that started it; when several buck2
daemons find it missing at once, a lock file in the directory makes one of them
start it while the others wait for the socket. Its output goes to
`buck2-casd.log` and its pid to `buck2-casd.pid`, both in the directory. To run
it under a service manager instead, start it yourself and set
`cas_shared_cache_autostart = false`:

```sh
$ buck2-casd --dir /var/cache/buck2-casd \
    --upstream grpc://cas.example.com:443 --upstream-tls \
    --max-size-bytes 53687091200 \
    --digest-function sha256
```

- `cas_shared_cache` - the daemon's `--dir`. Blobs found there are cloned into
  `buck-out`; buck2 never writes to it. Environment variables in `$VAR` form are
  substituted. Unset disables directory access.
- `cas_shared_cache_address` - only needed to move the daemon off its default
  socket: `unix:///path/to/socket`, or a loopback TCP port number (the only
  option on Windows, which has no Unix sockets). Whenever a daemon is
  configured, all CAS traffic goes to it in place of `cas_address`, without TLS.
  Engine and action cache traffic still goes to the addresses configured for
  them.
- `cas_shared_cache_copy_policy` - how blobs are cloned out of the directory.
  `hybrid` (the default) reflinks where the filesystem supports it and copies
  otherwise; `reflink` fails instead of falling back; `copy` always copies.
- `cas_shared_cache_mode` - `local_without_sync` (the default) clones from the
  directory and only fetches over gRPC when the daemon does not have a blob yet;
  `remote` never reads the directory and only talks gRPC to the daemon.
- `cas_shared_cache_autostart` - start the daemon on demand (the default).
- `cas_shared_cache_binary` - the `buck2-casd` executable to start, if it is not
  next to `buck2` or on `PATH`. Environment variables in `$VAR` form are
  substituted.
- `cas_shared_cache_max_size_bytes` - the size cap given to an auto-started
  daemon. Unset means it never evicts.

On a miss buck2 asks the daemon for the first byte of the blob, which makes the
daemon fetch and store all of it, and then clones it from the directory, so
even the first daemon to need a blob gets a shared copy rather than a private
one. If the blob still is not in the directory, buck2 falls back to receiving it
over gRPC.

Disk space is only shared when the clone is a reflink, which needs the daemon's
directory and `buck-out` to be on the same btrfs, XFS or APFS filesystem. On
other filesystems, or across filesystems, buck2 copies out of the directory: the
daemons still share the network fetch and the daemon's store, but not the
extents in `buck-out`. With the `hybrid` policy the buck2 daemon logs a warning
the first time it has to fall back. The daemon and the buck2 daemons must run
as users that can read each other's files.

Files buck2 uploads (sources and locally built outputs) pass through the daemon
too, so a second buck2 daemon that gets an action cache hit for the same action
clones the outputs without a download. The daemon verifies the hash of every
blob it stores, whether it came from a client or from upstream.

Eviction is the daemon's job. With a size cap it removes least recently
used blobs in the background once the store exceeds the cap; a clone by buck2
counts as a use. Sizes are nominal: a blob reflinked into a `buck-out` shares
its extents with that clone, and removing it from the store frees the space
only once every clone is gone too. Without a cap nothing is ever removed.
Hits and misses are reported in the `local_cache_hits_files` and related fields
of the invocation record.

Without `--upstream` the daemon is a standalone CAS, which is handy for tests
and for a purely local setup. Its other flags mirror the `[buck2_re_client]`
keys for TLS certificates, HTTP headers and the instance name used upstream;
see `buck2-casd --help`.

One thing this does not change: an action's digest includes its output paths,
and those contain the isolation directory (`buck-out/<isolation-dir>/...`), so
two isolation directories never get action cache hits from each other. What
they do share is content: any blob one of them uploads or fetches, such as an
identical output produced by both, is stored once in the daemon and cloned into
each `buck-out` on demand. Action cache hits, and with them full materialization
from the daemon's directory, happen across checkouts of the same repository,
after `buck2 clean`, and across daemon restarts.

## RE platform configuration

Next, your build will need an
[execution platform](https://buck2.build/docs/concepts/glossary/#execution-platform)
that specifies how and where actions should be executed. For a sample platform
definition that sets up an execution platform to utilize RE, take a look at the
[EngFlow example](https://github.com/facebook/buck2/blob/main/examples/remote_execution/engflow/platforms/defs.bzl),
[BuildBarn example](https://github.com/facebook/buck2/blob/main/examples/remote_execution/buildbarn/platforms/defs.bzl),
or the
[BuildBuddy example](https://github.com/facebook/buck2/blob/main/examples/remote_execution/buildbuddy/platforms/defs.bzl).

To enable remote execution, configure the following fields in
[CommandExecutorConfig](https://buck2.build/docs/api/build/globals/#commandexecutorconfig)
as follows:

- `remote_enabled` - set to `True`.
- `local_enabled` - set to `True` if you also want to run actions locally.
- `use_limited_hybrid` - set to `False` unless you want to exclusively run
  remotely when possible.
- `remote_execution_properties` - other additional properties.
  - If the RE engine requires a container image, this can be done by setting
    `container-image` to an image URL, as is done in the example above.
