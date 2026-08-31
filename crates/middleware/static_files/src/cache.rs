//! A file cache for one worker process.
//!
//! `ServeDir` makes all filesystem calls through the `Backend` trait, and this module
//! implements that trait. `ServeDir` keeps the ETag, the preconditions, Range and the header
//! set. This module supplies only the bytes and the metadata.
//!
//! An entry is fresh for one second. Each forked worker has its own cache.
//!
//! The cache does not store a miss. A new file is therefore visible on the next request.
//! Only a change to a file that is already in the cache can give stale data.
//!
//! The cache treats a file as changed when the mtime or the length is different. The ETag
//! encodes the same two values.
//!
//! A change of permissions does not make an entry stale. `stat` needs search permission on
//! the parent directory, not read permission on the file. To withdraw a file, delete it,
//! replace it, or remove search permission on its directory.
//!
//! `stat` runs on a runtime thread. The root must therefore be on local storage.

use std::collections::HashMap;
use std::future::{Ready, ready};
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};
use tower_http::services::fs::{Backend, File, Metadata};

/// The cache does not store a larger file. `ServeDir` streams it from disk.
const MAX_FILE: u64 = 256 * 1024;
/// The memory limit for one worker process. Forked workers do not share the cache.
const MAX_TOTAL: usize = 16 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(1);
/// The cache adds this value to the size of each entry. It covers the entry and the map slot.
/// A root of many small files needs it to keep the count near the true memory use.
const ENTRY_OVERHEAD: usize = 256;

#[derive(Clone, Copy)]
pub(crate) struct CachedMeta {
    is_dir: bool,
    modified: Option<SystemTime>,
    len: u64,
}

impl CachedMeta {
    fn new(meta: &std::fs::Metadata) -> Self {
        Self {
            is_dir: meta.is_dir(),
            modified: meta.modified().ok(),
            len: meta.len(),
        }
    }

    /// `ServeDir` makes the ETag and `Last-Modified` from these two values. Equal values
    /// therefore show that the cached body is still correct.
    fn same_file(&self, other: &Self) -> bool {
        self.modified == other.modified && self.len == other.len
    }
}

impl Metadata for CachedMeta {
    fn is_dir(&self) -> bool {
        self.is_dir
    }

    /// `ServeDir` calls `.ok()` on this result. An absent mtime gives no ETag and no
    /// `Last-Modified`. `std::fs::Metadata` gives the same answer for the same file.
    fn modified(&self) -> io::Result<SystemTime> {
        self.modified
            .ok_or_else(|| io::Error::other("modification time is not available"))
    }

    fn len(&self) -> u64 {
        self.len
    }
}

/// The file keeps its own metadata. `File::metadata` therefore makes no syscall, and the
/// bytes and the validators always come from one file descriptor.
pub(crate) enum CachedFile {
    Memory {
        cursor: Cursor<Bytes>,
        meta: CachedMeta,
    },
    Disk {
        file: tokio::fs::File,
        meta: CachedMeta,
    },
}

impl AsyncRead for CachedFile {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            CachedFile::Memory { cursor, .. } => Pin::new(cursor).poll_read(cx, buf),
            CachedFile::Disk { file, .. } => Pin::new(file).poll_read(cx, buf),
        }
    }
}

impl AsyncSeek for CachedFile {
    fn start_seek(self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        match self.get_mut() {
            CachedFile::Memory { cursor, .. } => Pin::new(cursor).start_seek(position),
            CachedFile::Disk { file, .. } => Pin::new(file).start_seek(position),
        }
    }

    fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        match self.get_mut() {
            CachedFile::Memory { cursor, .. } => Pin::new(cursor).poll_complete(cx),
            CachedFile::Disk { file, .. } => Pin::new(file).poll_complete(cx),
        }
    }
}

impl File for CachedFile {
    type Metadata = CachedMeta;
    type MetadataFuture<'a> = Ready<io::Result<CachedMeta>>;

    fn metadata(&self) -> Self::MetadataFuture<'_> {
        let meta = match self {
            CachedFile::Memory { meta, .. } | CachedFile::Disk { meta, .. } => *meta,
        };
        ready(Ok(meta))
    }
}

struct Entry {
    body: Option<Bytes>,
    meta: CachedMeta,
    checked: Instant,
    footprint: usize,
}

#[derive(Default)]
struct Store {
    map: HashMap<PathBuf, Entry>,
    bytes: usize,
}

impl Store {
    fn fresh(&self, path: &Path, now: Instant) -> Option<&Entry> {
        self.map
            .get(path)
            .filter(|e| now.duration_since(e.checked) < TTL)
    }

    fn take(&mut self, path: &Path) {
        if let Some(entry) = self.map.remove(path) {
            self.bytes -= entry.footprint;
        }
    }

    fn footprint(path: &Path, body: usize) -> usize {
        body + path.as_os_str().len() + ENTRY_OVERHEAD
    }

    /// A full cache continues to serve, but it stores no more entries. A replacement first
    /// releases the size of the old entry. A reload of the same size therefore always fits.
    fn has_room(&self, path: &Path, body: u64) -> bool {
        let reclaimed = self.map.get(path).map_or(0, |e| e.footprint);
        self.bytes - reclaimed + Self::footprint(path, body as usize) <= MAX_TOTAL
    }

    fn put(&mut self, path: PathBuf, body: Option<Bytes>, meta: CachedMeta, now: Instant) {
        self.take(&path);
        let footprint = Self::footprint(&path, body.as_ref().map_or(0, Bytes::len));
        if self.bytes + footprint > MAX_TOTAL {
            return;
        }
        self.bytes += footprint;
        self.map.insert(
            path,
            Entry {
                body,
                meta,
                checked: now,
                footprint,
            },
        );
    }
}

#[derive(Clone, Default)]
pub(crate) struct CachingBackend {
    store: Arc<Mutex<Store>>,
}

impl CachingBackend {
    /// The critical section contains only map operations and integer arithmetic. A poisoned
    /// lock therefore cannot mean that the store is in a bad state.
    fn lock(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stat(&self, path: &Path) -> io::Result<CachedMeta> {
        let now = Instant::now();
        if let Some(entry) = self.lock().fresh(path, now) {
            return Ok(entry.meta);
        }
        let meta = match std::fs::metadata(path) {
            Ok(meta) => CachedMeta::new(&meta),
            Err(err) => {
                self.lock().take(path);
                return Err(err);
            }
        };
        let mut store = self.lock();
        match store.map.get_mut(path) {
            Some(entry) if entry.meta.same_file(&meta) => entry.checked = now,
            _ => store.put(path.to_path_buf(), None, meta, now),
        }
        Ok(meta)
    }

    fn hit(&self, path: &Path) -> Option<CachedFile> {
        let store = self.lock();
        let entry = store.fresh(path, Instant::now())?;
        let body = entry.body.clone()?;
        Some(CachedFile::Memory {
            cursor: Cursor::new(body),
            meta: entry.meta,
        })
    }

    async fn fill(self, path: PathBuf) -> io::Result<CachedFile> {
        let mut file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(err) => {
                self.lock().take(&path);
                return Err(err);
            }
        };
        let meta = CachedMeta::new(&file.metadata()?);

        // `ServeDir` streams a file that the cache cannot store. The metadata entry stays, so
        // a later request needs no stat. `metadata` removes an entry that became stale.
        if meta.is_dir || meta.len > MAX_FILE || !self.lock().has_room(&path, meta.len) {
            return Ok(CachedFile::Disk {
                file: tokio::fs::File::from_std(file),
                meta,
            });
        }

        // Only the body read uses the blocking pool, and only once for each change to a file.
        let (mut file, buf, reread) = tokio::task::spawn_blocking(move || {
            let mut buf = Vec::with_capacity(meta.len as usize);
            file.read_to_end(&mut buf)?;
            let reread = file.metadata()?;
            io::Result::Ok((file, buf, CachedMeta::new(&reread)))
        })
        .await
        .map_err(io::Error::other)??;

        // The second stat detects a write that occurred during the read. An entry with new
        // metadata and old bytes stays stale: each revalidation compares the new metadata
        // with itself, and finds no change until the file changes again.
        if !reread.same_file(&meta) || buf.len() as u64 != meta.len {
            self.lock().take(&path);
            file.seek(SeekFrom::Start(0))?;
            return Ok(CachedFile::Disk {
                file: tokio::fs::File::from_std(file),
                meta: reread,
            });
        }

        let body = Bytes::from(buf);
        self.lock()
            .put(path, Some(body.clone()), meta, Instant::now());
        Ok(CachedFile::Memory {
            cursor: Cursor::new(body),
            meta,
        })
    }
}

impl Backend for CachingBackend {
    type File = CachedFile;
    type Metadata = CachedMeta;
    type OpenFuture = Pin<Box<dyn Future<Output = io::Result<CachedFile>> + Send>>;
    type MetadataFuture = Ready<io::Result<CachedMeta>>;

    /// This function is synchronous. A miss must not use the blocking pool.
    fn metadata(&self, path: PathBuf) -> Self::MetadataFuture {
        ready(self.stat(&path))
    }

    fn open(&self, path: PathBuf) -> Self::OpenFuture {
        if let Some(file) = self.hit(&path) {
            return Box::pin(ready(Ok(file)));
        }
        let backend = self.clone();
        Box::pin(async move { backend.fill(path).await })
    }
}

#[cfg(test)]
impl CachingBackend {
    pub(crate) fn accounted(&self) -> usize {
        self.lock().bytes
    }

    /// Adds the size of each entry again. The result must equal the running total.
    pub(crate) fn recomputed(&self) -> usize {
        self.lock().map.values().map(|e| e.footprint).sum()
    }

    pub(crate) fn entries(&self) -> usize {
        self.lock().map.len()
    }

    /// The number of entries that hold a body. A directory and a file above `MAX_FILE` store
    /// metadata only.
    pub(crate) fn bodies(&self) -> usize {
        self.lock()
            .map
            .values()
            .filter(|e| e.body.is_some())
            .count()
    }
}
