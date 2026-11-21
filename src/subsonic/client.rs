use anyhow::{Context, Result, anyhow, bail};
use md5::{Digest, Md5};
use rand::Rng;
use serde::de::DeserializeOwned;
use serde_json::Value;
use url::Url;

use super::models::{AlbumWithSongs, PlaylistWithSongs, Song};
use crate::config::StreamFormat;

const API_VERSION: &str = "1.16.1";
const CLIENT_NAME: &str = "psysonic-headless";

/// Stream quality, as in Psysonic: a bitrate cap (0 = original) and the transcode format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quality {
    pub max_bitrate: u32,
    pub format: StreamFormat,
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
            http: reqwest::Client::builder().user_agent(CLIENT_NAME).build()?,
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

    /// The stream URL for a song, with the quality options (nothing added for the original).
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

    /// The whole stream of a song.
    pub async fn fetch(&self, id: &str) -> Result<Vec<u8>> {
        let response = self
            .http
            .get(self.stream_url(id))
            .send()
            .await
            .context("could not reach the server for a stream")?
            .error_for_status()?;
        if response
            .headers()
            .get("content-type")
            .is_some_and(|t| t.as_bytes().starts_with(b"application/json"))
        {
            bail!("the server refused to stream {id}");
        }
        Ok(response.bytes().await?.to_vec())
    }
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
