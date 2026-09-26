/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! The buck2 gRPC client starting the real `buck2-casd` binary on demand.

use std::time::Duration;

use buck2_casd::Config;
use buck2_casd::DEFAULT_SOCKET_NAME;
use buck2_casd::Listen;
use buck2_casd::PID_FILE_NAME;
use buck2_casd::digest::DigestFunction;
use buck2_re_configuration::Buck2OssReConfiguration;
use remote_execution::DownloadRequest;
use remote_execution::InlinedBlobWithDigest;
use remote_execution::NamedDigest;
use remote_execution::NamedDigestWithPermissions;
use remote_execution::REClientBuilder;
use remote_execution::RemoteExecutionMetadata;
use remote_execution::TDigest;
use remote_execution::UploadRequest;

#[tokio::test(flavor = "multi_thread")]
async fn client_autostarts_the_daemon() -> anyhow::Result<()> {
    let work = tempfile::tempdir()?;

    // The remote: a standalone daemon in-process.
    let origin = buck2_casd::start(Config {
        dir: work.path().join("origin"),
        listen: Listen::Loopback(0),
        digest_function: DigestFunction::Sha256,
        max_size_bytes: None,
        upstream: None,
        eviction_interval: Duration::from_secs(3600),
    })
    .await?;
    let remote = origin.address.to_string();

    // No address configured: the daemon is expected at its default socket in the directory.
    let casd_dir = work.path().join("casd");

    let opts = Buck2OssReConfiguration {
        cas_address: Some(remote.clone()),
        engine_address: Some(remote.clone()),
        action_cache_address: Some(remote),
        tls: false,
        capabilities: Some(true),
        cas_shared_cache: Some(casd_dir.to_str().unwrap().to_owned()),
        cas_shared_cache_binary: Some(env!("CARGO_BIN_EXE_buck2-casd").to_owned()),
        cas_shared_cache_max_size_bytes: Some(1 << 30),
        digest_algorithm: Some("sha256".to_owned()),
        ..Default::default()
    };

    // Building the client is what starts the daemon.
    let client = REClientBuilder::build_and_connect(&opts).await?;
    let pid_file = casd_dir.join(PID_FILE_NAME);
    let pid_contents = std::fs::read_to_string(&pid_file)?;
    let pid: u32 = pid_contents.lines().next().unwrap().parse()?;
    assert!(pid != std::process::id());
    assert!(casd_dir.join("buck2-casd.log").exists());
    assert!(casd_dir.join(DEFAULT_SOCKET_NAME).exists());
    assert!(
        pid_contents.contains(&format!(
            "unix://{}",
            casd_dir.join(DEFAULT_SOCKET_NAME).display()
        )),
        "{pid_contents}"
    );

    let cleanup = || {
        #[cfg(unix)]
        {
            // SAFETY: plain libc call with a pid we just read from the file the daemon wrote.
            unsafe {
                libc_kill(pid as i32);
            }
        }
    };

    let outcome = async {
        // Upload through the auto-started daemon: it passes the blob to the origin.
        let data = b"started on demand".to_vec();
        let digest = TDigest {
            hash: DigestFunction::Sha256.hash_bytes(&data),
            size_in_bytes: data.len() as i64,
            ..Default::default()
        };
        client
            .upload(
                &RemoteExecutionMetadata::default(),
                UploadRequest {
                    inlined_blobs_with_digest: Some(vec![InlinedBlobWithDigest {
                        digest: digest.clone(),
                        blob: data.clone(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
            )
            .await?;
        assert!(
            origin
                .store
                .lookup_for_test(&digest.hash, digest.size_in_bytes)
                .await?
        );

        // A second client sees the daemon already running and reuses it.
        let again = REClientBuilder::build_and_connect(&opts).await?;
        let out = work.path().join("out");
        again
            .download(
                &RemoteExecutionMetadata::default(),
                DownloadRequest {
                    file_digests: Some(vec![NamedDigestWithPermissions {
                        named_digest: NamedDigest {
                            name: out.to_str().unwrap().to_owned(),
                            digest,
                            ..Default::default()
                        },
                        is_executable: false,
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
            )
            .await?;
        assert_eq!(std::fs::read(&out)?, data);
        assert_eq!(
            std::fs::read_to_string(&pid_file)?,
            pid_contents,
            "not restarted"
        );
        anyhow::Ok(())
    }
    .await;

    cleanup();
    origin.shutdown().await?;
    outcome
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        kill(pid, 15);
    }
}
