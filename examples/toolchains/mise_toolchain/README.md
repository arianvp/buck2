# Hermetic toolchains from a mise lock file

This project builds C++, Erlang, Go, Java, Kotlin, Python and Rust (and runs
Node in a genrule) without any of those tools installed. Every toolchain is
downloaded from the URLs and checked against the checksums that
[mise](https://mise.jdx.dev) recorded in its lock file, using the rules in
`@prelude//toolchains/mise:defs.bzl`.

- `mise.toml` pins the tools, as for any mise project.
- `mise.lock.toml` is the lock file written by `mise lock`. `mise.lock` is a
  symlink to it: Buck2 can only `load()` TOML files named `*.toml`, and mise
  writes through the symlink.
- `toolchains/BUCK` turns the lock file into toolchains.

```sh
buck2 run //go:hello
buck2 run //java:hello_kotlin
```

To update a tool, change its version in `mise.toml` and run `mise lock`; Buck2
picks up the new lock file on the next build.
