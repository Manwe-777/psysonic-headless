use anyhow::{Context, Result, anyhow, bail};
use md5::{Digest, Md5};
use rand::Rng;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tracing::debug;
use url::Url;

use std::sync::Arc;
use std::time::Duration;

use super::models::{AlbumWithSongs, PlayQueue, PlayQueueResponse, PlaylistWithSongs, Song};
use crate::audio::{Download, RangeFetcher, StreamSource, stream};
use crate::cache::{Cache, Kind, Writer};
use crate::config::StreamFormat;

const API_VERSION: &str = "1.16.1";
const CLIENT_NAME: &str = "psysonic-headless";

/// Stream quality, as in Psysonic: a bitrate cap (0 = original) and the transcode format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quality {
    pub max_bitrate: u32,
    pub format: StreamFormat,
}

impl Quality {
    /// No transcoding: the stream is the original file.
    pub fn is_original(&self) -> bool {
        self.max_bitrate == 0 && self.format == StreamFormat::Auto
    }
}

/// A client for one server account.
#[derive(Clone)]
pub struct Subsonic {
    http: reqwest::Client,
    base: Url,
    user: String,
    password: String,
    pub quality: Quality,
}

impl Subsonic {
    pub fn new(server_url: &str, user: &str, password: &str, quality: Quality) -> Result<Self> {
        let mut base = Url::parse(server_url).with_context(|| format!("not a valid server address: {server_url}"))?;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent(CLIENT_NAME)
                .connect_timeout(std::time::Duration::from_secs(10))
                .read_timeout(std::time::Duration::from_secs(30))
                .build()?,
            base,
            user: user.to_string(),
            password: password.to_string(),
            quality,
        })
    }

    /// The URL of an endpoint with authentication (a fresh salted token each time) and `params`.
    pub fn url(&self, endpoint: &str, params: &[(&str, &str)]) -> Url {
        let salt: String = {
            let mut rng = rand::rng();
            (0..12).map(|_| format!("{:x}", rng.random_range(0..16u8))).collect()
        };
        let token = hex(&Md5::digest(format!("{}{}", self.password, salt)));
        let mut url = self
            .base
            .join(&format!("rest/{endpoint}"))
            .expect("endpoint names are valid paths");
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("u", &self.user)
                .append_pair("t", &token)
                .append_pair("s", &salt)
                .append_pair("v", API_VERSION)
                .append_pair("c", CLIENT_NAME)
                .append_pair("f", "json");
            for (k, v) in params {
                q.append_pair(k, v);
            }
        }
        url
    }

    /// Calls an endpoint and returns the `subsonic-response` member named `key` (or the whole
    /// response when `key` is empty), turning a server error into an error.
    async fn get<T: DeserializeOwned>(&self, endpoint: &str, params: &[(&str, &str)], key: &str) -> Result<T> {
        let body: Value = self
            .http
            .get(self.url(endpoint, params))
            .send()
            .await
            .with_context(|| format!("could not reach the server for {endpoint}"))?
            .error_for_status()?
            .json()
            .await?;
        let response = body
            .get("subsonic-response")
            .ok_or_else(|| anyhow!("not a Subsonic server"))?;
        if response.get("status").and_then(Value::as_str) != Some("ok") {
            let message = response
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            bail!("the server refused {endpoint}: {message}");
        }
        let value = if key.is_empty() {
            response.clone()
        } else {
            response.get(key).cloned().unwrap_or(Value::Null)
        };
        serde_json::from_value(value).with_context(|| format!("unexpected answer to {endpoint}"))
    }

    /// Checks the address and the login.
    /// Any endpoint's answer as JSON (`key` picks one field of the response).
    pub async fn raw(&self, endpoint: &str, params: &[(&str, &str)], key: &str) -> Result<Value> {
        self.get(endpoint, params, key).await
    }

    pub async fn ping(&self) -> Result<()> {
        self.get::<Value>("ping.view", &[], "").await.map(|_| ())
    }

    pub async fn album_songs(&self, id: &str) -> Result<Vec<Song>> {
        Ok(self
            .get::<AlbumWithSongs>("getAlbum.view", &[("id", id)], "album")
            .await?
            .song)
    }

    pub async fn playlist_songs(&self, id: &str) -> Result<Vec<Song>> {
        Ok(self
            .get::<PlaylistWithSongs>("getPlaylist.view", &[("id", id)], "playlist")
            .await?
            .entry)
    }

    pub async fn song(&self, id: &str) -> Result<Song> {
        self.get("getSong.view", &[("id", id)], "song").await
    }

    /// The queue saved on the server (what handoffs between devices go through): its songs, the
    /// current song's id and the position in it.
    pub async fn play_queue(&self) -> Result<PlayQueue> {
        let queue: Option<PlayQueueResponse> = self.get("getPlayQueue.view", &[], "playQueue").await?;
        let queue = queue.unwrap_or_default();
        Ok(PlayQueue {
            songs: queue.entry,
            current: queue.current,
            position: Duration::from_millis(queue.position.unwrap_or(0)),
        })
    }

    /// Saves the queue on the server, as a form post (a long queue doesn't fit in a URL).
    pub async fn save_play_queue(&self, song_ids: &[String], current: Option<&str>, position: Duration) -> Result<()> {
        let mut form: Vec<(&str, String)> = song_ids.iter().map(|id| ("id", id.clone())).collect();
        if let Some(current) = current {
            form.push(("current", current.to_string()));
            form.push(("position", position.as_millis().to_string()));
        }
        let body: Value = self
            .http
            .post(self.url("savePlayQueue.view", &[]))
            .form(&form)
            .send()
            .await
            .context("could not reach the server to save the queue")?
            .error_for_status()?
            .json()
            .await?;
        match body.pointer("/subsonic-response/status").and_then(Value::as_str) {
            Some("ok") => Ok(()),
            _ => bail!("the server refused to save the queue"),
        }
    }

    /// Cover art at about `size` pixels: its bytes and media type.
    pub async fn cover_art(&self, id: &str, size: u32) -> Result<(Vec<u8>, String)> {
        let size = size.to_string();
        let response = self
            .http
            .get(self.url("getCoverArt.view", &[("id", id), ("size", &size)]))
            .send()
            .await?
            .error_for_status()?;
        let mime = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if !mime.starts_with("image/") {
            bail!("no cover art for {id}");
        }
        Ok((response.bytes().await?.to_vec(), mime))
    }

    /// The stream URL for a song, with the quality options (nothing added for the original).
    pub fn quality(&self) -> &Quality {
        &self.quality
    }

    /// A track for the cache: the original file (`format=raw`, as the desktop asks for it) or the
    /// stream at the current settings.
    pub async fn download(&self, id: &str, original: bool) -> Result<reqwest::Response> {
        let url = if original {
            self.url("stream.view", &[("id", id), ("format", "raw")])
        } else {
            self.stream_url(id)
        };
        let response = self.http.get(url).send().await?.error_for_status()?;
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if content_type.starts_with("application/json") || content_type.starts_with("text/xml") {
            bail!("the server refused to send {id}");
        }
        Ok(response)
    }

    pub fn stream_url(&self, id: &str) -> Url {
        let bitrate = self.quality.max_bitrate.to_string();
        let mut params = vec![("id", id)];
        if self.quality.max_bitrate > 0 {
            params.push(("maxBitRate", &bitrate));
        }
        if let Some(format) = self.quality.format.param() {
            params.push(("format", format));
        }
        self.url("stream.view", &params)
    }

    /// Opens a song's stream: returns once the server answers, while the file keeps downloading in
    /// the background, with the container type the server sent (for a transcode that differs from
    /// the original file's).
    ///
    /// `keep`: also write the stream to the cache as this kind (an original only if its size is
    /// the original's).
    pub async fn open_stream(
        &self,
        song: &Song,
        keep: Option<(Arc<Cache>, Kind)>,
    ) -> Result<(StreamSource, Option<&'static str>)> {
        let id = song.id.as_str();
        let response = self
            .http
            .get(self.stream_url(id))
            .send()
            .await
            .context("could not reach the server for a stream")?
            .error_for_status()?;
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if content_type.starts_with("application/json") || content_type.starts_with("text/xml") {
            bail!("the server refused to stream {id}");
        }
        let hint = container_hint(content_type);
        let len = response.content_length().filter(|n| *n > 0);
        let takes_ranges = response
            .headers()
            .get("accept-ranges")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("bytes"));
        let ranges: Option<Arc<dyn RangeFetcher>> = (takes_ranges && len.is_some()).then(|| {
            Arc::new(HttpRanges {
                http: self.http.clone(),
                url: response.url().clone(),
                runtime: tokio::runtime::Handle::current(),
            }) as Arc<dyn RangeFetcher>
        });
        let (download, source) = stream(len, ranges);
        let keep = match (keep, len) {
            (Some((cache, kind)), Some(len)) if kind != Kind::Original || song.size.is_none_or(|s| s == len) => {
                let ext = if kind == Kind::Original {
                    song.suffix.clone()
                } else {
                    hint.map(str::to_string)
                };
                cache.writer(id, kind, ext.as_deref()).await.ok().map(|w| (w, len))
            }
            _ => None,
        };
        tokio::spawn(fill(response, download, keep));
        Ok((source, hint))
    }
}

/// Fetches parts of a stream with ranged requests (for seeks ahead of the download).
struct HttpRanges {
    http: reqwest::Client,
    url: Url,
    runtime: tokio::runtime::Handle,
}

impl RangeFetcher for HttpRanges {
    fn fetch(&self, download: Download, start: u64, end: u64) {
        let (http, url) = (self.http.clone(), self.url.clone());
        self.runtime.spawn(async move {
            let result = async {
                let mut response = http
                    .get(url)
                    .header("range", format!("bytes={start}-{}", end - 1))
                    .send()
                    .await?
                    .error_for_status()?;
                if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
                    bail!("the server ignored the range");
                }
                let mut at = start;
                while at < end && !download.abandoned() {
                    let Some(chunk) = response.chunk().await? else { break };
                    let take = chunk.len().min((end - at) as usize);
                    download.write_at(at, &chunk[..take]);
                    at += take as u64;
                }
                anyhow::Ok(())
            }
            .await;
            if let Err(e) = result {
                debug!("ranged fetch {start}-{end} failed: {e:#}");
                tokio::time::sleep(Duration::from_secs(1)).await; // don't hammer a failing server
            }
            download.range_finished(start, end);
        });
    }
}

/// Downloads into the stream until the end, an error, or the reader went away.
/// Downloads into the stream until the end, an error, or the reader went away; with `keep`, also
/// into the cache (kept only when it all arrived).
async fn fill(mut response: reqwest::Response, download: Download, mut keep: Option<(Writer, u64)>) {
    loop {
        if download.abandoned() {
            return;
        }
        match response.chunk().await {
            Ok(Some(chunk)) => {
                download.append(&chunk);
                if let Some((writer, _)) = &mut keep
                    && let Err(e) = writer.write(&chunk).await
                {
                    debug!("not caching this track: {e:#}");
                    keep = None;
                }
            }
            Ok(None) => {
                download.finish();
                if let Some((writer, len)) = keep
                    && let Err(e) = writer.commit(len).await
                {
                    debug!("not caching this track: {e:#}");
                }
                return;
            }
            Err(e) => return download.fail(e.to_string()),
        }
    }
}

/// The file extension for a stream's media type, as a format hint for the decoder.
pub fn container_hint(content_type: &str) -> Option<&'static str> {
    let mime = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    Some(match mime.as_str() {
        "audio/ogg" | "application/ogg" | "audio/opus" => "ogg",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/mp4" | "audio/x-m4a" | "audio/m4a" => "m4a",
        "audio/aac" | "audio/aacp" => "aac",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/aiff" | "audio/x-aiff" => "aiff",
        _ => return None,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(quality: Quality) -> Subsonic {
        Subsonic::new("https://music.example.com/navidrome", "me", "secret", quality).unwrap()
    }
    const ORIGINAL: Quality = Quality {
        max_bitrate: 0,
        format: StreamFormat::Auto,
    };

    fn param(url: &Url, name: &str) -> Option<String> {
        url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned())
    }

    #[test]
    fn requests_carry_a_salted_token_never_the_password() {
        let url = client(ORIGINAL).url("ping.view", &[]);
        assert_eq!(url.path(), "/navidrome/rest/ping.view");
        let salt = param(&url, "s").unwrap();
        assert_eq!(param(&url, "t").unwrap(), hex(&Md5::digest(format!("secret{salt}"))));
        assert!(!url.as_str().contains("secret"));
        assert_eq!(param(&url, "u").as_deref(), Some("me"));
    }

    #[test]
    fn a_transcoded_stream_is_decoded_as_what_the_server_sent() {
        assert_eq!(container_hint("audio/ogg"), Some("ogg"));
        assert_eq!(container_hint("audio/mpeg; charset=binary"), Some("mp3"));
        assert_eq!(container_hint("audio/x-flac"), Some("flac"));
        assert_eq!(container_hint("application/octet-stream"), None);
    }

    #[test]
    fn the_original_stream_asks_for_no_transcoding() {
        let url = client(ORIGINAL).stream_url("s1");
        assert_eq!(param(&url, "id").as_deref(), Some("s1"));
        assert_eq!(param(&url, "maxBitRate"), None);
        assert_eq!(param(&url, "format"), None);
    }

    #[test]
    fn a_quality_cap_and_format_are_passed_on_like_psysonic() {
        let url = client(Quality {
            max_bitrate: 192,
            format: StreamFormat::Opus,
        })
        .stream_url("s1");
        assert_eq!(param(&url, "maxBitRate").as_deref(), Some("192"));
        assert_eq!(param(&url, "format").as_deref(), Some("opus"));
    }
}
