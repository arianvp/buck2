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
of every blob they need. Configuring a local CAS cache gives them one shared
store instead:

```ini
[buck2_re_client]
cas_local_cache = $HOME/.cache/buck2/cas
cas_local_cache_max_size_bytes = 21474836480
```

- `cas_local_cache` - absolute path of a directory shared by every daemon on the
  machine that is configured with the same value. Blobs are downloaded into it
  once and then cloned into whichever `buck-out` needs them. Environment
  variables in `$VAR` form are substituted. Unset disables the cache.
- `cas_local_cache_copy_policy` - how blobs leave the cache. `hybrid` (the
  default) makes a copy-on-write clone where the filesystem supports it and
  falls back to a plain copy otherwise; `reflink` fails instead of falling back;
  `copy` always copies.
- `cas_local_cache_max_size_bytes` - size cap for the cache. Once it is
  exceeded, least recently used blobs are removed in the background until the
  cache is a tenth below the cap. Unset means nothing is ever removed.

Disk space is only shared when the clone is a reflink, which needs the cache and
`buck-out` to be on the same btrfs, XFS or APFS filesystem. On other
filesystems, or across filesystems, buck2 still copies out of the cache, so
several daemons save the network fetch but not the disk. With the `hybrid`
policy the daemon logs a warning the first time it has to fall back.

Files buck2 uploads to the CAS (sources and locally built outputs) are added to
the cache as well when they can be reflinked, so a second daemon that gets an
action cache hit for the same action materializes the outputs without a
download. Under the `copy` policy uploads are never added.

Sizes counted against the cap are nominal: a blob that has been reflinked into
a `buck-out` shares its extents with that copy, and removing it from the cache
frees the space only once every clone is gone too. Any number of daemons can
use one cache at the same time; blobs appear through an atomic rename, so no
coordinating process is needed. Hits and misses are reported in the
`local_cache_hits_files` and related fields of the invocation record.

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
