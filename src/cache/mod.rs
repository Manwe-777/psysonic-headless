//! The track cache on disk, after the Psysonic desktop's: upcoming tracks are prefetched, played
//! tracks are kept, and the least recently played go first when it is full (never the track playing
//! or the next one).
//!
//! Layout: `<dir>/<server key>/<track id>.<quality>.<ext>`, where the server key covers the server
//! address and the user (as on the hub) and the quality is `original` or the stream settings
//! (`320k-auto`). Files are written to a `.part` first and renamed once their size checks out.
//! The modification time is the last time a track was played.

mod prefetch;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

pub use prefetch::spawn_prefetch;

use crate::config::{CacheMode, StreamFormat};
use crate::subsonic::{Quality, Song};

/// What a cache entry holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// The original file, as stored on the server: it plays whatever the stream settings are.
    Original,
    /// A stream as played with the current settings (a transcode).
    Streamed,
}

pub struct Cache {
    dir: PathBuf,
    mode: CacheMode,
    max_bytes: u64,
    quality: Quality,
    /// Song ids never evicted: the track playing and the next one.
    protected: Mutex<HashSet<String>>,
    /// One eviction at a time (two working from the same listing would remove too much).
    evicting: Mutex<()>,
}

/// A cached track ready to play.
pub struct Hit {
    pub path: PathBuf,
    /// The container's file extension.
    pub hint: Option<String>,
}

impl Cache {
    /// The cache for one server and user, or none when it is off.
    pub fn open(
        root: &Path,
        server_key: &str,
        mode: CacheMode,
        max_mb: u64,
        quality: Quality,
    ) -> Result<Option<Arc<Self>>> {
        if mode == CacheMode::Off || max_mb == 0 {
            return Ok(None);
        }
        let dir = root.join(&server_key[..16.min(server_key.len())]);
        std::fs::create_dir_all(&dir).with_context(|| format!("could not create the cache in {}", dir.display()))?;
        // Leftovers of downloads that never finished.
        for entry in std::fs::read_dir(&dir)?.flatten() {
            if entry.file_name().to_string_lossy().ends_with(".part") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let cache = Arc::new(Self {
            dir,
            mode,
            max_bytes: max_mb * 1024 * 1024,
            quality,
            protected: Mutex::new(HashSet::new()),
            evicting: Mutex::new(()),
        });
        info!(dir = %cache.dir.display(), mode = ?mode, max_mb, used_mb = cache.used() / (1024 * 1024), "track cache");
        cache.evict();
        Ok(Some(cache))
    }

    /// What prefetching fetches: originals, or streams at the current settings.
    pub fn prefetch_kind(&self) -> Kind {
        match self.mode {
            CacheMode::Streamed if !self.quality.is_original() => Kind::Streamed,
            _ => Kind::Original,
        }
    }

    /// What a played stream can be kept as, if anything: in `original` mode only a stream that is
    /// the original file; in `streamed` mode any.
    pub fn stream_kind(&self) -> Option<Kind> {
        match (self.mode, self.quality.is_original()) {
            (_, true) => Some(Kind::Original),
            (CacheMode::Streamed, false) => Some(Kind::Streamed),
            _ => None,
        }
    }

    fn tag(&self, kind: &Kind) -> String {
        match kind {
            Kind::Original => "original".into(),
            Kind::Streamed => {
                let format = match self.quality.format {
                    StreamFormat::Auto => "auto",
                    StreamFormat::Mp3 => "mp3",
                    StreamFormat::Opus => "opus",
                    StreamFormat::Aac => "aac",
                };
                format!("{}k-{format}", self.quality.max_bitrate)
            }
        }
    }

    fn prefix(&self, song_id: &str, kind: &Kind) -> String {
        format!("{}.{}.", file_id(song_id), self.tag(kind))
    }

    /// A cached copy of the song to play: the original if there is one (it plays whatever the
    /// settings), otherwise, in `streamed` mode, one streamed with the current settings.
    pub fn lookup(&self, song_id: &str) -> Option<Hit> {
        let mut kinds = vec![Kind::Original];
        if self.mode == CacheMode::Streamed {
            kinds.push(Kind::Streamed);
        }
        let names: Vec<String> = std::fs::read_dir(&self.dir)
            .ok()?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.ends_with(".part"))
            .collect();
        kinds.iter().find_map(|kind| {
            let prefix = self.prefix(song_id, kind);
            let name = names.iter().find(|n| n.starts_with(&prefix))?;
            let ext = &name[prefix.len()..];
            Some(Hit {
                path: self.dir.join(name),
                hint: (!ext.is_empty() && ext != "bin").then(|| ext.to_string()),
            })
        })
    }

    pub fn contains(&self, song_id: &str) -> bool {
        self.lookup(song_id).is_some()
    }

    /// Marks a cached track as just played (eviction removes the least recently played first).
    pub fn touch(&self, path: &Path) {
        if let Ok(file) = std::fs::File::options().write(true).open(path) {
            let _ = file.set_modified(SystemTime::now());
        }
    }

    /// The track playing and the next one: never evicted.
    pub fn protect(&self, song_ids: impl IntoIterator<Item = String>) {
        *self.protected.lock().unwrap() = song_ids.into_iter().collect();
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Starts writing a cache entry; it only appears once [`Writer::commit`] checks its size.
    pub async fn writer(self: &Arc<Self>, song_id: &str, kind: Kind, ext: Option<&str>) -> Result<Writer> {
        let name = format!("{}{}", self.prefix(song_id, &kind), ext.unwrap_or("bin"));
        let path = self.dir.join(&name);
        let part = self.dir.join(format!("{name}.part"));
        let file = tokio::fs::File::create(&part).await?;
        Ok(Writer {
            cache: self.clone(),
            song_id: song_id.to_string(),
            kind,
            path,
            part,
            file: Some(file),
            written: 0,
        })
    }

    /// Bytes the cached tracks take.
    pub fn used(&self) -> u64 {
        self.files().iter().map(|(_, len, _)| len).sum()
    }

    fn files(&self) -> Vec<(PathBuf, u64, SystemTime)> {
        let Ok(dir) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        dir.flatten()
            .filter(|e| !e.file_name().to_string_lossy().ends_with(".part"))
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                meta.is_file()
                    .then(|| (e.path(), meta.len(), meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)))
            })
            .collect()
    }

    /// Removes the least recently played tracks until the cache fits (never a protected one).
    pub fn evict(&self) {
        let _one_at_a_time = self.evicting.lock().unwrap();
        let protected: Vec<String> = self
            .protected
            .lock()
            .unwrap()
            .iter()
            .map(|id| format!("{}.", file_id(id)))
            .collect();
        let mut files = self.files();
        let mut used: u64 = files.iter().map(|(_, len, _)| len).sum();
        files.sort_by_key(|(_, _, played)| *played);
        for (path, len, _) in files {
            if used <= self.max_bytes {
                break;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if protected.iter().any(|p| name.starts_with(p)) {
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    used -= len;
                    debug!(file = %name, "evicted from the cache");
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => used -= len,
                Err(e) => warn!("could not evict {}: {e}", path.display()),
            }
        }
    }
}

/// A cache entry being written.
pub struct Writer {
    cache: Arc<Cache>,
    song_id: String,
    kind: Kind,
    path: PathBuf,
    part: PathBuf,
    file: Option<tokio::fs::File>,
    written: u64,
}

impl Writer {
    pub async fn write(&mut self, chunk: &[u8]) -> Result<()> {
        if let Some(file) = &mut self.file {
            file.write_all(chunk).await?;
            self.written += chunk.len() as u64;
        }
        Ok(())
    }

    /// Keeps the file if exactly `expected` bytes were written (the size the server announced, and
    /// for an original also the original file's size); throws it away otherwise.
    pub async fn commit(mut self, expected: u64) -> Result<PathBuf> {
        let mut file = self.file.take().context("already committed")?;
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        if self.written != expected || expected == 0 {
            anyhow::bail!("got {} bytes of {expected}", self.written);
        }
        tokio::fs::rename(&self.part, &self.path).await?;
        debug!(song = %self.song_id, kind = ?self.kind, bytes = self.written, "cached");
        let cache = self.cache.clone();
        tokio::task::spawn_blocking(move || cache.evict());
        Ok(self.path.clone())
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if self.file.is_some() || self.part.exists() {
            let _ = std::fs::remove_file(&self.part);
        }
    }
}

/// A file-name-safe form of a track id (ids that need changing get a hash, so two never collide).
fn file_id(id: &str) -> String {
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return id.to_string();
    }
    let safe: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).take(40).collect();
    let hash = Sha256::digest(id.as_bytes());
    format!("{safe}_{}", hex(&hash[..6]))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The cache's place on disk: `--cache-dir`, systemd's `CACHE_DIRECTORY`, `$XDG_CACHE_HOME` or
/// `~/.cache`.
pub fn default_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CACHE_DIRECTORY").filter(|d| !d.is_empty()) {
        // systemd's CacheDirectory= may list several, separated by ':'.
        let first = dir.to_string_lossy().split(':').next().unwrap_or_default().to_string();
        return PathBuf::from(first);
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("psysonic-headless")
}

/// What a song will take in the cache, roughly (to keep prefetching within the size limit).
pub fn estimate(song: &Song, kind: &Kind, quality: &Quality) -> u64 {
    let seconds = u64::from(song.duration.unwrap_or(240));
    match kind {
        Kind::Original => song.size.unwrap_or(seconds * 1411 * 1000 / 8),
        Kind::Streamed => seconds * u64::from(quality.max_bitrate) * 1000 / 8 * 106 / 100,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quality(max_bitrate: u32) -> Quality {
        Quality {
            max_bitrate,
            format: StreamFormat::Auto,
        }
    }

    fn cache(mode: CacheMode, max_mb: u64, q: Quality) -> (Arc<Cache>, PathBuf) {
        let root = std::env::temp_dir().join(format!("psh-cache-{}-{}", std::process::id(), rand::random::<u32>()));
        (
            Cache::open(&root, "0123456789abcdef0123", mode, max_mb, q)
                .unwrap()
                .unwrap(),
            root,
        )
    }

    async fn put(cache: &Arc<Cache>, id: &str, kind: Kind, ext: &str, len: usize) -> PathBuf {
        let mut w = cache.writer(id, kind, Some(ext)).await.unwrap();
        w.write(&vec![7u8; len]).await.unwrap();
        w.commit(len as u64).await.unwrap()
    }

    #[test]
    fn off_or_no_room_means_no_cache() {
        let root = std::env::temp_dir();
        assert!(
            Cache::open(&root, "k", CacheMode::Off, 256, quality(0))
                .unwrap()
                .is_none()
        );
        assert!(
            Cache::open(&root, "k", CacheMode::Original, 0, quality(0))
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_original_plays_whatever_the_stream_settings() {
        let (c, root) = cache(CacheMode::Original, 256, quality(320));
        put(&c, "s1", Kind::Original, "flac", 100).await;
        let hit = c.lookup("s1").unwrap();
        assert_eq!(hit.hint.as_deref(), Some("flac"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_stream_kept_at_other_settings_is_not_played() {
        let (c320, root) = cache(CacheMode::Streamed, 256, quality(320));
        put(&c320, "s1", Kind::Streamed, "ogg", 100).await;
        assert!(c320.lookup("s1").is_some());
        let c128 = Cache::open(&root, "0123456789abcdef0123", CacheMode::Streamed, 256, quality(128))
            .unwrap()
            .unwrap();
        assert!(
            c128.lookup("s1").is_none(),
            "a 320 kbps copy must not stand in for 128 kbps"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_original_streams_are_kept_in_original_mode() {
        let (c, root) = cache(CacheMode::Original, 256, quality(320));
        assert_eq!(c.stream_kind(), None);
        assert_eq!(c.prefetch_kind(), Kind::Original);
        let (s, root2) = cache(CacheMode::Streamed, 256, quality(320));
        assert_eq!(s.stream_kind(), Some(Kind::Streamed));
        let (o, root3) = cache(CacheMode::Streamed, 256, quality(0));
        assert_eq!(
            o.stream_kind(),
            Some(Kind::Original),
            "an uncapped stream is the original"
        );
        for r in [root, root2, root3] {
            std::fs::remove_dir_all(r).unwrap();
        }
    }

    #[tokio::test]
    async fn a_short_download_is_thrown_away() {
        let (c, root) = cache(CacheMode::Original, 256, quality(0));
        let mut w = c.writer("s1", Kind::Original, Some("mp3")).await.unwrap();
        w.write(&[1, 2, 3]).await.unwrap();
        assert!(w.commit(10).await.is_err());
        assert!(c.lookup("s1").is_none());
        assert_eq!(std::fs::read_dir(&c.dir).unwrap().count(), 0, "no .part left behind");
        let w = c.writer("s2", Kind::Original, Some("mp3")).await.unwrap();
        drop(w); // a skipped track
        assert_eq!(std::fs::read_dir(&c.dir).unwrap().count(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn played_at(path: &Path, ago_secs: u64) {
        let at = SystemTime::now() - std::time::Duration::from_secs(ago_secs);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }

    #[tokio::test]
    async fn the_least_recently_played_go_first_but_never_the_protected_ones() {
        let (c, root) = cache(CacheMode::Original, 1, quality(0)); // 1 MB
        let half = 1024 * 1024 / 2 - 10;
        let playing = put(&c, "playing", Kind::Original, "mp3", half).await;
        let old = put(&c, "old", Kind::Original, "mp3", half).await;
        let recent = put(&c, "recent", Kind::Original, "mp3", 10).await; // all three just fit
        played_at(&playing, 7200);
        played_at(&old, 3600);
        played_at(&recent, 60);
        c.protect(["playing".to_string()]);
        put(&c, "new", Kind::Original, "mp3", half).await;
        c.evict();
        assert!(
            c.contains("playing"),
            "the track playing stays even though it was played longest ago"
        );
        assert!(!c.contains("old"));
        assert!(c.contains("recent"), "only as much as needed goes");
        assert!(c.contains("new"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn playing_a_cached_track_makes_it_recent() {
        let (c, root) = cache(CacheMode::Original, 256, quality(0));
        let path = put(&c, "s1", Kind::Original, "mp3", 10).await;
        played_at(&path, 3600);
        c.touch(&path);
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(modified.elapsed().unwrap() < std::time::Duration::from_secs(60));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn odd_track_ids_become_safe_and_distinct_file_names() {
        assert_eq!(file_id("5GNsjON3NUF2wqRqYCaeaK"), "5GNsjON3NUF2wqRqYCaeaK");
        assert!(!file_id("../etc/passwd").contains('/'));
        assert_ne!(file_id("a/b"), file_id("a:b"));
    }
}
