/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! `buck2-casd`: a machine-local content-addressed storage daemon for open-source buck2.
//!
//! It speaks the remote execution API's CAS and ByteStream services, passes misses and uploads
//! through to a real remote CAS, and keeps every blob it has seen as a raw, read-only file in a
//! directory it alone owns. Buck2 daemons on the same machine point their CAS traffic at it and,
//! given the directory, materialize outputs by reflinking those files instead of receiving bytes
//! over gRPC. One store, one downloader, one eviction policy, any number of isolation dirs.
//!
//! Run without an upstream it is a complete standalone cache backend (CAS plus action cache) for
//! local builds that should share their outputs across isolation dirs without any remote.

pub mod digest;
pub mod server;
pub mod store;
pub mod upstream;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use re_grpc_proto::build::bazel::remote::execution::v2::action_cache_server::ActionCacheServer;
use re_grpc_proto::build::bazel::remote::execution::v2::capabilities_server::CapabilitiesServer;
use re_grpc_proto::build::bazel::remote::execution::v2::content_addressable_storage_server::ContentAddressableStorageServer;
use re_grpc_proto::google::bytestream::byte_stream_server::ByteStreamServer;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::digest::DigestFunction;
use crate::server::Cas;
use crate::store::Store;
use crate::upstream::Upstream;
use crate::upstream::UpstreamConfig;

/// gRPC message size limit. Batch payloads are capped well below this.
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Config {
    /// Root of the store. Buck2 daemons are given this same path as `cas_shared_cache`.
    pub dir: PathBuf,
    /// Address to serve on. Port 0 picks a free port; see [`Running::local_addr`].
    pub listen: SocketAddr,
    pub digest_function: DigestFunction,
    /// Size cap for the store; `None` never evicts.
    pub max_size_bytes: Option<u64>,
    /// The remote CAS to pass through to. `None` makes this a standalone CAS.
    pub upstream: Option<UpstreamConfig>,
    /// How often to check the cap when nothing is being written.
    pub eviction_interval: Duration,
}

/// A running daemon.
pub struct Running {
    pub local_addr: SocketAddr,
    pub store: Arc<Store>,
    shutdown: Option<oneshot::Sender<()>>,
    server: JoinHandle<Result<(), tonic::transport::Error>>,
    evictor: JoinHandle<()>,
}

impl Running {
    /// Stops serving and waits for in-flight requests to finish.
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        if let Some(tx) = self.shutdown.take() {
            let _ignored = tx.send(());
        }
        self.evictor.abort();
        (&mut self.server)
            .await
            .context("Server task panicked")?
            .context("Server failed")
    }

    /// Waits until the server exits on its own.
    pub async fn wait(mut self) -> anyhow::Result<()> {
        self.evictor.abort();
        (&mut self.server)
            .await
            .context("Server task panicked")?
            .context("Server failed")
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ignored = tx.send(());
        }
        self.evictor.abort();
    }
}

pub async fn start(config: Config) -> anyhow::Result<Running> {
    let store = Arc::new(Store::open(
        &config.dir,
        config.digest_function,
        config.max_size_bytes,
    )?);
    let upstream = match &config.upstream {
        Some(upstream) => Some(Arc::new(Upstream::connect(upstream).await?)),
        None => None,
    };
    let cas = Cas::new(Arc::clone(&store), upstream);

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("Error binding `{}`", config.listen))?;
    let local_addr = listener.local_addr()?;
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let router = tonic::transport::Server::builder()
        .add_service(
            ContentAddressableStorageServer::new(cas.clone())
                .max_decoding_message_size(MAX_MESSAGE_SIZE)
                .max_encoding_message_size(MAX_MESSAGE_SIZE),
        )
        .add_service(
            ByteStreamServer::new(cas.clone())
                .max_decoding_message_size(MAX_MESSAGE_SIZE)
                .max_encoding_message_size(MAX_MESSAGE_SIZE),
        )
        .add_service(ActionCacheServer::new(cas.clone()))
        .add_service(CapabilitiesServer::new(cas));
    let server = tokio::spawn(router.serve_with_incoming_shutdown(incoming, async {
        let _ignored = shutdown_rx.await;
    }));

    let evictor = tokio::spawn({
        let store = Arc::clone(&store);
        let interval = config.eviction_interval;
        async move {
            loop {
                tokio::time::sleep(interval).await;
                if let Err(e) = store.evict().await {
                    tracing::warn!("Eviction failed: {e:#}");
                }
            }
        }
    });

    tracing::info!(
        "buck2-casd serving on {local_addr}, store `{}`, upstream {}",
        config.dir.display(),
        config
            .upstream
            .as_ref()
            .map_or("none (standalone)".to_owned(), |u| u.address.clone())
    );
    Ok(Running {
        local_addr,
        store,
        shutdown: Some(shutdown_tx),
        server,
        evictor,
    })
}
