/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! A machine-local, content-addressed blob store shared by every buck2 daemon that points at
//! the same directory.
//!
//! Buck2 keeps all of its own on-disk state under `buck-out/<isolation-dir>`, so two daemons
//! with different isolation dirs (or two checkouts of the same repo) never share a byte. This
//! store sits in front of the remote CAS: a blob is downloaded once into the store and then
//! reflinked (copy-on-write cloned) into each `buck-out` that asks for it. On a filesystem with
//! reflink support (btrfs, XFS, APFS) that means the data exists once on disk no matter how many
//! daemons materialize it. Where reflinks are unavailable it degrades to a plain copy, which still
//! saves the network round trip.
//!
//! Layout on disk:
//!
//! ```text
//! <root>/blobs/<first two hex chars>/<hash>-<size>   completed blobs, read-only
//! <root>/tmp/<random>                                 in-flight downloads
//! ```
//!
//! Blobs become visible through an atomic rename, so any number of processes can safely use one
//! store concurrently without a daemon coordinating them. Eviction is least-recently-used by
//! modification time: a hit touches the blob, and a size cap (if configured) removes the oldest
//! blobs in the background once the store grows past it.

use std::fs;
use std::fs::File;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;

use anyhow::Context;
use buck2_re_configuration::CopyPolicy;

use crate::digest::TDigest;
use crate::response::TLocalCacheStats;

/// In-flight downloads older than this are assumed to belong to a dead process and are removed.
const STALE_TMP_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Mode for blobs at rest. Nothing should ever write to a stored blob.
#[cfg(unix)]
const STORED_BLOB_MODE: u32 = 0o444;

pub struct LocalCasCache {
    blobs_dir: PathBuf,
    tmp_dir: PathBuf,
    copy_policy: CopyPolicy,
    max_size_bytes: Option<u64>,
    /// Bytes written into the store since the last eviction pass was started.
    bytes_since_eviction: AtomicU64,
    /// Set while an eviction pass is running so only one runs at a time per process.
    eviction_running: Arc<AtomicBool>,
    /// Under the hybrid policy, set once a reflink failed because the filesystem cannot do it,
    /// so later materializations go straight to copying.
    reflink_unsupported: Arc<AtomicBool>,
}

impl std::fmt::Debug for LocalCasCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalCasCache")
            .field("blobs_dir", &self.blobs_dir)
            .field("copy_policy", &self.copy_policy)
            .field("max_size_bytes", &self.max_size_bytes)
            .finish()
    }
}

/// Hit and miss counters for one download request. The writes that update them run
/// concurrently, hence the atomics.
#[derive(Default)]
pub struct LocalCacheCounters {
    hits_files: AtomicI64,
    hits_bytes: AtomicI64,
    misses_files: AtomicI64,
    misses_bytes: AtomicI64,
}

impl LocalCacheCounters {
    pub fn hit(&self, bytes: i64) {
        self.hits_files.fetch_add(1, Ordering::Relaxed);
        self.hits_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn miss(&self, bytes: i64) {
        self.misses_files.fetch_add(1, Ordering::Relaxed);
        self.misses_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn take_stats(&self) -> TLocalCacheStats {
        let hits_files = self.hits_files.load(Ordering::Relaxed);
        let misses_files = self.misses_files.load(Ordering::Relaxed);
        TLocalCacheStats {
            total_cache_lookup_attempts: hits_files + misses_files,
            hits_files,
            hits_bytes: self.hits_bytes.load(Ordering::Relaxed),
            misses_files,
            misses_bytes: self.misses_bytes.load(Ordering::Relaxed),
            ..Default::default()
        }
    }
}

impl LocalCasCache {
    /// Opens (creating if needed) the store rooted at `root`.
    pub fn new(
        root: PathBuf,
        copy_policy: CopyPolicy,
        max_size_bytes: Option<u64>,
    ) -> anyhow::Result<Self> {
        if !root.is_absolute() {
            return Err(anyhow::anyhow!(
                "The local CAS cache path must be absolute, got `{}`",
                root.display()
            ));
        }
        let blobs_dir = root.join("blobs");
        let tmp_dir = root.join("tmp");
        fs::create_dir_all(&blobs_dir)
            .with_context(|| format!("Error creating `{}`", blobs_dir.display()))?;
        fs::create_dir_all(&tmp_dir)
            .with_context(|| format!("Error creating `{}`", tmp_dir.display()))?;
        remove_stale_tmp_files(&tmp_dir);

        let cache = Self {
            blobs_dir,
            tmp_dir,
            copy_policy,
            max_size_bytes,
            bytes_since_eviction: AtomicU64::new(0),
            eviction_running: Arc::new(AtomicBool::new(false)),
            reflink_unsupported: Arc::new(AtomicBool::new(false)),
        };
        // Pretend a full interval has elapsed so the first insert reconciles whatever a previous
        // process left behind against the cap.
        cache
            .bytes_since_eviction
            .store(cache.eviction_check_interval(), Ordering::Relaxed);
        Ok(cache)
    }

    pub fn copy_policy(&self) -> CopyPolicy {
        self.copy_policy
    }

    /// Where a blob with this digest lives, whether or not it is present.
    fn blob_path(&self, digest: &TDigest) -> anyhow::Result<PathBuf> {
        let hash = digest.hash.as_str();
        // The hash becomes a path component, so refuse anything that is not plain hex.
        if hash.is_empty() || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(anyhow::anyhow!(
                "Refusing to use non-hex digest hash `{}` as a local cache key",
                hash
            ));
        }
        if digest.size_in_bytes < 0 {
            return Err(anyhow::anyhow!("Digest `{}` has a negative size", digest));
        }
        let shard = &hash[..hash.len().min(2)];
        Ok(self
            .blobs_dir
            .join(shard)
            .join(format!("{}-{}", hash, digest.size_in_bytes)))
    }

    /// Materializes `digest` at `dst` from the store if it is present.
    ///
    /// Returns `Ok(true)` on a hit, `Ok(false)` on a miss. Errors are reserved for a blob that is
    /// present but could not be materialized under the configured copy policy.
    pub async fn materialize(
        &self,
        digest: &TDigest,
        dst: &Path,
        executable: bool,
    ) -> anyhow::Result<bool> {
        let blob_path = self.blob_path(digest)?;
        let expected_size = digest.size_in_bytes as u64;
        let dst = dst.to_owned();
        let copy_policy = self.copy_policy;
        let reflink_unsupported = Arc::clone(&self.reflink_unsupported);

        // The store operations are all plain syscalls; keep them off the async executor.
        blocking(move || {
            let src = match File::open(&blob_path) {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("Error opening `{}`", blob_path.display()));
                }
            };
            let meta = src
                .metadata()
                .with_context(|| format!("Error stat-ing `{}`", blob_path.display()))?;
            if meta.len() != expected_size {
                // Never rename a partial file into place, so this means something else damaged
                // the store. Drop the entry and re-download.
                tracing::warn!(
                    "Local CAS cache entry `{}` has size {} but digest says {}; removing it",
                    blob_path.display(),
                    meta.len(),
                    expected_size
                );
                let _ignored = fs::remove_file(&blob_path);
                return Ok(false);
            }

            // Best-effort LRU bookkeeping.
            let _ignored = src.set_modified(SystemTime::now());

            link_into(
                &src,
                &blob_path,
                &dst,
                executable,
                copy_policy,
                &reflink_unsupported,
            )?;
            Ok(true)
        })
        .await
    }

    /// Path for a new in-flight download. The caller writes the blob there and then calls
    /// [`Self::commit`].
    pub fn new_tmp_path(&self) -> PathBuf {
        self.tmp_dir.join(uuid::Uuid::new_v4().to_string())
    }

    /// Moves a fully written temporary file into the store under `digest`.
    pub async fn commit(&self, digest: &TDigest, tmp_path: PathBuf) -> anyhow::Result<()> {
        let blob_path = self.blob_path(digest)?;
        let expected_size = digest.size_in_bytes as u64;
        let digest = digest.clone();
        blocking(move || {
            let result = (|| {
                let meta = fs::metadata(&tmp_path)
                    .with_context(|| format!("Error stat-ing `{}`", tmp_path.display()))?;
                if meta.len() != expected_size {
                    return Err(anyhow::anyhow!(
                        "Downloaded {} bytes for digest `{}`, expected {}",
                        meta.len(),
                        digest,
                        expected_size
                    ));
                }
                make_read_only(&tmp_path)?;
                if let Some(parent) = blob_path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("Error creating `{}`", parent.display()))?;
                }
                // If another process won the race the rename replaces identical content.
                fs::rename(&tmp_path, &blob_path).with_context(|| {
                    format!(
                        "Error moving `{}` to `{}`",
                        tmp_path.display(),
                        blob_path.display()
                    )
                })?;
                Ok(())
            })();
            if result.is_err() {
                let _ignored = fs::remove_file(&tmp_path);
            }
            result
        })
        .await?;
        self.record_inserted(expected_size);
        Ok(())
    }

    /// Stores an in-memory blob under `digest`.
    pub async fn insert_from_bytes(&self, digest: &TDigest, data: &[u8]) -> anyhow::Result<()> {
        let tmp_path = self.new_tmp_path();
        let data = data.to_vec();
        let write_path = tmp_path.clone();
        blocking(move || {
            fs::write(&write_path, data)
                .with_context(|| format!("Error writing `{}`", write_path.display()))
        })
        .await?;
        self.commit(digest, tmp_path).await
    }

    /// Adds a file that is about to be uploaded to the store, so that another daemon fetching
    /// the same digest later finds it locally.
    ///
    /// The file is only ever reflinked, never copied: on a copy-on-write filesystem this costs
    /// nothing, and on any other filesystem it would double the disk usage of every uploaded
    /// input, which is the opposite of what the store is for. Returns whether the file was added.
    pub async fn insert_from_path(&self, digest: &TDigest, src: &Path) -> anyhow::Result<bool> {
        if digest.size_in_bytes == 0
            || matches!(self.copy_policy, CopyPolicy::Copy)
            || self.reflink_unsupported.load(Ordering::Relaxed)
        {
            return Ok(false);
        }
        let blob_path = self.blob_path(digest)?;
        let expected_size = digest.size_in_bytes as u64;
        let tmp_path = self.new_tmp_path();
        let src = src.to_owned();
        let inserted = blocking(move || {
            if blob_path.exists() {
                return Ok(false);
            }
            let src_file =
                File::open(&src).with_context(|| format!("Error opening `{}`", src.display()))?;
            let meta = src_file
                .metadata()
                .with_context(|| format!("Error stat-ing `{}`", src.display()))?;
            if !meta.is_file() || meta.len() != expected_size {
                return Ok(false);
            }
            match reflink(&src_file, &src, &tmp_path) {
                Ok(()) => {}
                Err(e) if is_reflink_unsupported(&e) => {
                    let _ignored = fs::remove_file(&tmp_path);
                    return Ok(false);
                }
                Err(e) => {
                    let _ignored = fs::remove_file(&tmp_path);
                    return Err(e).with_context(|| {
                        format!(
                            "Error reflinking `{}` into the local CAS cache",
                            src.display()
                        )
                    });
                }
            }
            let result = (|| {
                make_read_only(&tmp_path)?;
                if let Some(parent) = blob_path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("Error creating `{}`", parent.display()))?;
                }
                fs::rename(&tmp_path, &blob_path).with_context(|| {
                    format!(
                        "Error moving `{}` to `{}`",
                        tmp_path.display(),
                        blob_path.display()
                    )
                })
            })();
            if result.is_err() {
                let _ignored = fs::remove_file(&tmp_path);
            }
            result.map(|()| true)
        })
        .await?;
        if inserted {
            self.record_inserted(expected_size);
        }
        Ok(inserted)
    }

    /// How many bytes may be inserted between two eviction passes.
    fn eviction_check_interval(&self) -> u64 {
        const MIN: u64 = 8 << 20;
        const MAX: u64 = 1 << 30;
        match self.max_size_bytes {
            Some(max) => (max / 32).clamp(MIN, MAX),
            None => u64::MAX,
        }
    }

    fn record_inserted(&self, bytes: u64) {
        let Some(max_size_bytes) = self.max_size_bytes else {
            return;
        };
        let interval = self.eviction_check_interval();
        let since = self
            .bytes_since_eviction
            .fetch_add(bytes, Ordering::Relaxed)
            .saturating_add(bytes);
        if since < interval {
            return;
        }
        if self
            .eviction_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // Another pass is in flight; it will observe these bytes on disk.
            return;
        }
        self.bytes_since_eviction.store(0, Ordering::Relaxed);
        let blobs_dir = self.blobs_dir.clone();
        let running = Arc::clone(&self.eviction_running);
        tokio::task::spawn_blocking(move || {
            evict_blocking(&blobs_dir, max_size_bytes);
            running.store(false, Ordering::Release);
        });
    }
}

/// Runs a blocking filesystem operation on tokio's blocking pool.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .context("Local CAS cache task panicked")?
}

/// Removes least recently used blobs until the store is comfortably under `max_size_bytes`.
///
/// Sizes are nominal: a blob that has been reflinked into a `buck-out` shares its extents with
/// that copy, so removing it from the store frees space only once every clone is gone too.
pub(crate) fn evict_blocking(blobs_dir: &Path, max_size_bytes: u64) {
    let mut entries: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    let mut total: u64 = 0;

    let Ok(shards) = fs::read_dir(blobs_dir) else {
        return;
    };
    for shard in shards.flatten() {
        let Ok(blobs) = fs::read_dir(shard.path()) else {
            continue;
        };
        for blob in blobs.flatten() {
            let Ok(meta) = blob.metadata() else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            total = total.saturating_add(meta.len());
            entries.push((mtime, meta.len(), blob.path()));
        }
    }

    if total <= max_size_bytes {
        return;
    }

    // Free a little more than strictly necessary so this doesn't run again immediately.
    let target = max_size_bytes - max_size_bytes / 10;
    entries.sort_by_key(|entry| entry.0);

    let before = total;
    let mut removed_files = 0u64;
    for (_, len, path) in entries {
        if total <= target {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total -= len;
            removed_files += 1;
        }
    }
    tracing::info!(
        "Local CAS cache eviction removed {} blobs ({} -> {} bytes, cap {})",
        removed_files,
        before,
        total,
        max_size_bytes
    );
}

fn remove_stale_tmp_files(tmp_dir: &Path) {
    let Ok(entries) = fs::read_dir(tmp_dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let age = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .unwrap_or(Duration::ZERO);
        if age >= STALE_TMP_AGE {
            let _ignored = fs::remove_file(entry.path());
        }
    }
}

fn make_read_only(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(STORED_BLOB_MODE))
            .with_context(|| format!("Error setting permissions on `{}`", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let mut perms = fs::metadata(path)
            .with_context(|| format!("Error stat-ing `{}`", path.display()))?
            .permissions();
        perms.set_readonly(true);
        fs::set_permissions(path, perms)
            .with_context(|| format!("Error setting permissions on `{}`", path.display()))?;
    }
    Ok(())
}

fn set_output_permissions(path: &Path, executable: bool) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o755 } else { 0o644 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .with_context(|| format!("Error setting permissions on `{}`", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = executable;
        let mut perms = fs::metadata(path)
            .with_context(|| format!("Error stat-ing `{}`", path.display()))?
            .permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(path, perms)
            .with_context(|| format!("Error setting permissions on `{}`", path.display()))?;
    }
    Ok(())
}

/// Puts the contents of the stored blob `src` at `dst` according to the copy policy.
fn link_into(
    src: &File,
    src_path: &Path,
    dst: &Path,
    executable: bool,
    copy_policy: CopyPolicy,
    reflink_unsupported: &AtomicBool,
) -> anyhow::Result<()> {
    let try_reflink = match copy_policy {
        CopyPolicy::Copy => false,
        CopyPolicy::Reflink => true,
        CopyPolicy::Hybrid => !reflink_unsupported.load(Ordering::Relaxed),
    };

    if try_reflink {
        match reflink(src, src_path, dst) {
            Ok(()) => {
                set_output_permissions(dst, executable)?;
                return Ok(());
            }
            Err(e) if matches!(copy_policy, CopyPolicy::Hybrid) && is_reflink_unsupported(&e) => {
                if !reflink_unsupported.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        "Reflinking from the local CAS cache into `{}` is not supported ({}); \
                         falling back to copying. Blobs will not share disk space. Put the cache \
                         on the same reflink-capable filesystem as buck-out to fix this.",
                        dst.display(),
                        e
                    );
                }
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "Error reflinking `{}` to `{}` (copy policy is `reflink`; use `hybrid` \
                         to fall back to copying)",
                        src_path.display(),
                        dst.display()
                    )
                });
            }
        }
    }

    fs::copy(src_path, dst).with_context(|| {
        format!(
            "Error copying `{}` to `{}`",
            src_path.display(),
            dst.display()
        )
    })?;
    set_output_permissions(dst, executable)
}

/// Whether a failed reflink means "this filesystem (or this pair of filesystems) cannot do
/// that", as opposed to a genuine I/O error.
fn is_reflink_unsupported(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    let Some(code) = e.raw_os_error() else {
        return false;
    };
    #[cfg(unix)]
    {
        #[cfg(target_os = "macos")]
        if code == libc::ENOTSUP {
            return true;
        }
        matches!(
            code,
            libc::EOPNOTSUPP | libc::EXDEV | libc::EINVAL | libc::ENOSYS | libc::ENOTTY
        )
    }
    #[cfg(not(unix))]
    {
        let _ = code;
        true
    }
}

/// Creates `dst` as a copy-on-write clone of `src`.
#[cfg(target_os = "linux")]
fn reflink(src: &File, _src_path: &Path, dst: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let dst_file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)?;
    // SAFETY: FICLONE takes the source file descriptor as its only argument. Both descriptors
    // are open for the duration of the call and the kernel does not retain them.
    let rc = unsafe { libc::ioctl(dst_file.as_raw_fd(), libc::FICLONE as _, src.as_raw_fd()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn reflink(_src: &File, src_path: &Path, dst: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    // clonefile refuses to overwrite, so clear the destination first.
    match fs::remove_file(dst) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let src_c = CString::new(src_path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let dst_c = CString::new(dst.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: both arguments are valid NUL-terminated strings that outlive the call.
    let rc = unsafe { libc::clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn reflink(_src: &File, _src_path: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "reflinks are not supported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(hash: &str, size: i64) -> TDigest {
        TDigest {
            hash: hash.to_owned(),
            size_in_bytes: size,
            ..Default::default()
        }
    }

    fn cache(root: &Path, policy: CopyPolicy, max: Option<u64>) -> LocalCasCache {
        LocalCasCache::new(root.join("cas"), policy, max).unwrap()
    }

    #[tokio::test]
    async fn test_miss_then_hit() -> anyhow::Result<()> {
        let work = tempfile::tempdir()?;
        let cache = cache(work.path(), CopyPolicy::Hybrid, None);
        let d = digest("abcdef", 3);
        let dst = work.path().join("out").join("file");
        fs::create_dir_all(dst.parent().unwrap())?;

        assert!(!cache.materialize(&d, &dst, false).await?);
        assert!(!dst.exists());

        cache.insert_from_bytes(&d, b"xyz").await?;
        assert!(cache.materialize(&d, &dst, true).await?);
        assert_eq!(fs::read(&dst)?, b"xyz");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&dst)?.permissions().mode() & 0o777, 0o755);
            let stored = cache.blob_path(&d)?;
            assert_eq!(fs::metadata(&stored)?.permissions().mode() & 0o777, 0o444);
        }

        // Materializing a second time over an existing file works and fixes the mode.
        assert!(cache.materialize(&d, &dst, false).await?);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&dst)?.permissions().mode() & 0o777, 0o644);
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_copy_policy() -> anyhow::Result<()> {
        let work = tempfile::tempdir()?;
        let cache = cache(work.path(), CopyPolicy::Copy, None);
        let d = digest("0011", 4);
        cache.insert_from_bytes(&d, b"data").await?;
        let dst = work.path().join("dst");
        assert!(cache.materialize(&d, &dst, false).await?);
        assert_eq!(fs::read(&dst)?, b"data");

        // Copy policy never adds uploads to the store.
        let other = digest("0022", 4);
        fs::write(work.path().join("src"), b"more")?;
        assert!(
            !cache
                .insert_from_path(&other, &work.path().join("src"))
                .await?
        );
        assert!(!cache.blob_path(&other)?.exists());
        Ok(())
    }

    #[tokio::test]
    async fn test_strict_reflink_policy() -> anyhow::Result<()> {
        let work = tempfile::tempdir()?;
        let cache = cache(work.path(), CopyPolicy::Reflink, None);
        let d = digest("1234", 4);
        cache.insert_from_bytes(&d, b"data").await?;
        let dst = work.path().join("dst");
        // On a filesystem with reflink support this simply works; anywhere else the strict
        // policy must refuse to silently degrade to a copy.
        match cache.materialize(&d, &dst, false).await {
            Ok(true) => assert_eq!(fs::read(&dst)?, b"data"),
            Ok(false) => panic!("blob was just inserted"),
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(msg.contains("copy policy is `reflink`"), "{msg}");
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_insert_from_path_is_reflink_only() -> anyhow::Result<()> {
        let work = tempfile::tempdir()?;
        let cache = cache(work.path(), CopyPolicy::Hybrid, None);
        let src = work.path().join("src");
        fs::write(&src, b"hello")?;
        let d = digest("ff00", 5);
        // Whether this succeeds depends on the filesystem the tempdir is on; either way it must
        // not fail, and on success the blob must be usable.
        let inserted = cache.insert_from_path(&d, &src).await?;
        let dst = work.path().join("dst");
        assert_eq!(cache.materialize(&d, &dst, false).await?, inserted);
        if inserted {
            assert_eq!(fs::read(&dst)?, b"hello");
        }

        // A size mismatch is never inserted.
        let wrong = digest("ff01", 42);
        assert!(!cache.insert_from_path(&wrong, &src).await?);
        assert!(!cache.blob_path(&wrong)?.exists());
        Ok(())
    }

    #[tokio::test]
    async fn test_commit_rejects_size_mismatch() -> anyhow::Result<()> {
        let work = tempfile::tempdir()?;
        let cache = cache(work.path(), CopyPolicy::Hybrid, None);
        let d = digest("abcd", 10);
        let tmp = cache.new_tmp_path();
        fs::write(&tmp, b"short")?;
        assert!(cache.commit(&d, tmp.clone()).await.is_err());
        assert!(!tmp.exists());
        assert!(!cache.blob_path(&d)?.exists());
        Ok(())
    }

    #[tokio::test]
    async fn test_corrupt_entry_is_dropped() -> anyhow::Result<()> {
        let work = tempfile::tempdir()?;
        let cache = cache(work.path(), CopyPolicy::Hybrid, None);
        let d = digest("abcd", 10);
        let blob_path = cache.blob_path(&d)?;
        fs::create_dir_all(blob_path.parent().unwrap())?;
        fs::write(&blob_path, b"exactly 10")?; // 10 bytes: matches the digest
        assert!(cache.materialize(&d, &work.path().join("a"), false).await?);
        fs::write(&blob_path, b"tampered")?; // 8 bytes: mismatch
        assert!(!cache.materialize(&d, &work.path().join("b"), false).await?);
        assert!(!blob_path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn test_rejects_bad_hash() -> anyhow::Result<()> {
        let work = tempfile::tempdir()?;
        let cache = cache(work.path(), CopyPolicy::Hybrid, None);
        for bad in ["", "../../etc/passwd", "zz", "AB/CD"] {
            let d = digest(bad, 1);
            assert!(
                cache
                    .materialize(&d, &work.path().join("x"), false)
                    .await
                    .is_err(),
                "{bad:?} should be rejected"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_relative_root_rejected() {
        assert!(
            LocalCasCache::new(PathBuf::from("relative/cas"), CopyPolicy::Hybrid, None).is_err()
        );
    }

    #[tokio::test]
    async fn test_eviction_removes_oldest() -> anyhow::Result<()> {
        let work = tempfile::tempdir()?;
        let cache = cache(work.path(), CopyPolicy::Hybrid, Some(100));
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        for i in 0..10u64 {
            let d = digest(&format!("{i:02}ab"), 20);
            cache.insert_from_bytes(&d, &[0u8; 20]).await?;
            let path = cache.blob_path(&d)?;
            File::open(&path)?.set_modified(base + Duration::from_secs(i))?;
        }
        // 200 bytes stored against a cap of 100: evict down to 90.
        evict_blocking(&cache.blobs_dir, 100);
        let mut remaining: Vec<String> = Vec::new();
        for i in 0..10u64 {
            let d = digest(&format!("{i:02}ab"), 20);
            if cache.blob_path(&d)?.exists() {
                remaining.push(d.hash);
            }
        }
        assert_eq!(remaining, vec!["06ab", "07ab", "08ab", "09ab"]);
        Ok(())
    }

    #[tokio::test]
    async fn test_stale_tmp_cleanup() -> anyhow::Result<()> {
        let work = tempfile::tempdir()?;
        let root = work.path().join("cas");
        let tmp = root.join("tmp");
        fs::create_dir_all(&tmp)?;
        let stale = tmp.join("stale");
        let fresh = tmp.join("fresh");
        fs::write(&stale, b"x")?;
        fs::write(&fresh, b"y")?;
        File::open(&stale)?.set_modified(SystemTime::now() - STALE_TMP_AGE * 2)?;
        let _cache = LocalCasCache::new(root, CopyPolicy::Hybrid, None)?;
        assert!(!stale.exists());
        assert!(fresh.exists());
        Ok(())
    }

    #[test]
    fn test_counters() {
        let c = LocalCacheCounters::default();
        c.hit(10);
        c.hit(5);
        c.miss(7);
        let s = c.take_stats();
        assert_eq!(s.hits_files, 2);
        assert_eq!(s.hits_bytes, 15);
        assert_eq!(s.misses_files, 1);
        assert_eq!(s.misses_bytes, 7);
        assert_eq!(s.total_cache_lookup_attempts, 3);
    }
}
