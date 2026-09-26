/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use buck2_casd::Config;
use buck2_casd::digest::DigestFunction;
use buck2_casd::upstream::UpstreamConfig;
use clap::Parser;

/// A machine-local CAS daemon that fronts a remote CAS and shares its blobs, as raw files that
/// buck2 can reflink, with every buck2 daemon on this host.
#[derive(Parser, Debug)]
#[command(name = "buck2-casd", version)]
struct Args {
    /// Directory to keep blobs in. Give the same path to buck2 as
    /// `[buck2_re_client] cas_shared_cache`. It must be on the same filesystem as the
    /// repositories' `buck-out` for reflinks to work.
    #[arg(long)]
    dir: PathBuf,

    /// Address to listen on. Give the port to buck2 as `[buck2_re_client] cas_shared_cache_address`.
    #[arg(long, default_value = "127.0.0.1:9092")]
    listen: SocketAddr,

    /// Hash function blobs are addressed by; must match `[buck2] digest_algorithms`.
    #[arg(long, default_value = "sha256")]
    digest_function: DigestFunction,

    /// Evict least recently used blobs once the store exceeds this many bytes.
    #[arg(long)]
    max_size_bytes: Option<u64>,

    /// Seconds between periodic checks of the size cap.
    #[arg(long, default_value_t = 60)]
    eviction_interval_secs: u64,

    /// gRPC address of the remote CAS to pass through to, e.g. `grpc://cas.example.com:443`.
    /// Without it the daemon is a standalone CAS.
    #[arg(long)]
    upstream: Option<String>,

    /// Use TLS for the upstream connection.
    #[arg(long, default_value_t = false)]
    upstream_tls: bool,

    /// PEM bundle of CA certificates for the upstream connection.
    #[arg(long)]
    upstream_tls_ca_certs: Option<String>,

    /// PEM client certificate and key for the upstream connection.
    #[arg(long)]
    upstream_tls_client_cert: Option<String>,

    /// `Header: value` to add to every upstream request. Repeatable.
    #[arg(long = "upstream-http-header")]
    upstream_http_headers: Vec<String>,

    /// Instance name for upstream requests.
    #[arg(long)]
    upstream_instance_name: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let upstream = args.upstream.map(|address| UpstreamConfig {
        address,
        tls: args.upstream_tls,
        tls_ca_certs: args.upstream_tls_ca_certs,
        tls_client_cert: args.upstream_tls_client_cert,
        http_headers: args.upstream_http_headers,
        instance_name: args.upstream_instance_name,
    });
    let running = buck2_casd::start(Config {
        dir: args.dir,
        listen: args.listen,
        digest_function: args.digest_function,
        max_size_bytes: args.max_size_bytes,
        upstream,
        eviction_interval: Duration::from_secs(args.eviction_interval_secs),
    })
    .await?;

    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutting down");
    running.shutdown().await
}
