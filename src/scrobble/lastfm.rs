//! Last.fm (Audioscrobbler 2.0), like Psysonic's: the app's key is built in, the listener allows
//! it once in a browser (token flow) and the player keeps the session key it gets.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use md5::{Digest, Md5};
use serde_json::Value;

use crate::subsonic::Song;

const API: &str = "https://ws.audioscrobbler.com/2.0/";
const AUTH: &str = "https://www.last.fm/api/auth/";
/// How often, and how long, sign-in waits for the listener to allow access.
const POLL_EVERY: Duration = Duration::from_secs(3);
const POLL_FOR: Duration = Duration::from_secs(600);

/// The app's key and secret, built into release builds (CI repository secrets); empty otherwise.
const BUILT_IN_KEY: Option<&str> = option_env!("PSYSONIC_HEADLESS_LASTFM_KEY");
const BUILT_IN_SECRET: Option<&str> = option_env!("PSYSONIC_HEADLESS_LASTFM_SECRET");

/// An app key and secret: `PSYSONIC_LASTFM_API_KEY` and `PSYSONIC_LASTFM_API_SECRET` (or `_FILE`)
/// replace the built-in ones.
#[derive(Clone)]
pub struct App {
    key: String,
    secret: String,
}

impl App {
    pub fn get() -> Result<Self> {
        let from_env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let key =
            from_env("PSYSONIC_LASTFM_API_KEY").or_else(|| BUILT_IN_KEY.filter(|k| !k.is_empty()).map(Into::into));
        let secret = match from_env("PSYSONIC_LASTFM_API_SECRET") {
            Some(s) => Some(s),
            None => match from_env("PSYSONIC_LASTFM_API_SECRET_FILE") {
                Some(path) => Some(crate::config::read_secret_file(path.as_ref())?),
                None => BUILT_IN_SECRET.filter(|s| !s.is_empty()).map(Into::into),
            },
        };
        match (key, secret) {
            (Some(key), Some(secret)) => Ok(Self { key, secret }),
            _ => bail!(
                "this build has no Last.fm app key: set PSYSONIC_LASTFM_API_KEY and PSYSONIC_LASTFM_API_SECRET_FILE"
            ),
        }
    }
}

/// Why a call failed, and whether to try again.
#[derive(Debug)]
pub enum Failure {
    /// The network or Last.fm is down: the play is kept and sent later.
    Retry(String),
    /// Last.fm no longer takes the session (access was revoked): kept until signed in again.
    Session(String),
    /// Last.fm will never take it.
    Drop(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Retry(m) | Failure::Drop(m) => f.write_str(m),
            Failure::Session(m) => write!(f, "{m} (sign in again: sudo psysonic-headless install --lastfm)"),
        }
    }
}

/// A signed-in Last.fm account.
#[derive(Clone)]
pub struct LastFm {
    http: reqwest::Client,
    app: App,
    session: String,
}

impl LastFm {
    pub fn new(app: App, session: String) -> Result<Self> {
        Ok(Self {
            http: http()?,
            app,
            session,
        })
    }

    pub async fn now_playing(&self, song: &Song) -> Result<(), Failure> {
        let mut p = vec![("method", "track.updateNowPlaying".to_string())];
        p.extend(track_params(song, ""));
        p.push(("sk", self.session.clone()));
        call(&self.http, &self.app, p, false).await.map(|_| ())
    }

    /// Submits a play that started at `at` (Unix seconds).
    pub async fn scrobble(&self, song: &Song, at: i64) -> Result<(), Failure> {
        let mut p = vec![("method", "track.scrobble".to_string())];
        p.extend(track_params(song, "[0]"));
        p.push(("timestamp[0]", at.to_string()));
        p.push(("sk", self.session.clone()));
        let answer = call(&self.http, &self.app, p, false).await?;
        if answer
            .pointer("/scrobbles/@attr/ignored")
            .and_then(as_count)
            .unwrap_or(0)
            > 0
        {
            let why = answer
                .pointer("/scrobbles/scrobble/ignoredMessage/#text")
                .and_then(Value::as_str)
                .unwrap_or("ignored");
            return Err(Failure::Drop(format!("Last.fm ignored the play: {why}")));
        }
        Ok(())
    }
}

/// Signs in: shows the link where the listener allows access (`show` prints it), then waits for
/// that and returns the session key and the Last.fm user name.
pub async fn sign_in(app: &App, show: impl FnOnce(&str)) -> Result<(String, String)> {
    let http = http()?;
    let token = call(&http, app, vec![("method", "auth.getToken".to_string())], true)
        .await
        .map_err(|e| anyhow::anyhow!("Last.fm: {e}"))?
        .get("token")
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(token) = token else {
        bail!("Last.fm gave no sign-in token");
    };
    show(&format!("{AUTH}?api_key={}&token={token}", app.key));
    let until = Instant::now() + POLL_FOR;
    while Instant::now() < until {
        tokio::time::sleep(POLL_EVERY).await;
        let params = vec![("method", "auth.getSession".to_string()), ("token", token.clone())];
        match call(&http, app, params, false).await {
            Ok(answer) => {
                let key = answer.pointer("/session/key").and_then(Value::as_str);
                let name = answer.pointer("/session/name").and_then(Value::as_str);
                if let (Some(key), Some(name)) = (key, name) {
                    return Ok((key.to_string(), name.to_string()));
                }
            }
            // 14: not allowed yet.
            Err(Failure::Drop(m)) if m.starts_with("14:") => {}
            Err(Failure::Retry(_)) => {}
            Err(e) => bail!("Last.fm sign-in failed: {e}"),
        }
    }
    bail!("Last.fm access wasn't allowed in time: run it again")
}

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("psysonic-headless/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()?)
}

/// The song's fields for a scrobble (`suffix` "[0]" for the batch form of track.scrobble).
fn track_params(song: &Song, suffix: &str) -> Vec<(&'static str, String)> {
    let name = |base: &'static str| -> &'static str {
        match (base, suffix) {
            (b, "") => b,
            ("artist", _) => "artist[0]",
            ("track", _) => "track[0]",
            ("album", _) => "album[0]",
            _ => "duration[0]",
        }
    };
    let mut p = vec![
        (name("artist"), song.main_artist().unwrap_or_default().to_string()),
        (name("track"), song.title.clone()),
    ];
    if let Some(album) = song.album.as_ref().filter(|a| !a.is_empty()) {
        p.push((name("album"), album.clone()));
    }
    if let Some(d) = song.duration.filter(|d| *d > 0) {
        p.push((name("duration"), d.to_string()));
    }
    p
}

/// The request signature: every parameter but `format`, sorted by name, then the secret, MD5'd.
pub fn signature(params: &[(&str, String)], secret: &str) -> String {
    let mut sorted: Vec<_> = params
        .iter()
        .filter(|(k, _)| *k != "format" && *k != "callback")
        .collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut md5 = Md5::new();
    for (k, v) in sorted {
        md5.update(k.as_bytes());
        md5.update(v.as_bytes());
    }
    md5.update(secret.as_bytes());
    md5.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// A signed call: a GET for `get`, else a form POST.
async fn call(http: &reqwest::Client, app: &App, mut params: Vec<(&str, String)>, get: bool) -> Result<Value, Failure> {
    params.push(("api_key", app.key.clone()));
    let sig = signature(&params, &app.secret);
    params.push(("api_sig", sig));
    params.push(("format", "json".to_string()));
    let request = if get {
        let url = url::Url::parse_with_params(API, &params).expect("a valid address");
        http.get(url)
    } else {
        http.post(API).form(&params)
    };
    let response = request
        .send()
        .await
        .map_err(|e| Failure::Retry(format!("could not reach Last.fm: {e}")))?;
    let status = response.status();
    let body: Option<Value> = response.json().await.ok();
    match body {
        Some(body) => match body.get("error").and_then(Value::as_i64) {
            None if status.is_success() => Ok(body),
            None => Err(Failure::Retry(format!("Last.fm answered {status}"))),
            Some(code) => {
                let message = body.get("message").and_then(Value::as_str).unwrap_or("").to_string();
                Err(classify(code, format!("{code}: {message}")))
            }
        },
        None => Err(Failure::Retry(format!("Last.fm answered {status}"))),
    }
}

/// Last.fm error codes: 9 = the session was revoked; 11, 16 and 29 = try later.
fn classify(code: i64, message: String) -> Failure {
    match code {
        9 => Failure::Session(message),
        8 | 11 | 16 | 29 => Failure::Retry(message),
        _ => Failure::Drop(message),
    }
}

/// Last.fm sends counts as numbers or strings.
fn as_count(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(json: serde_json::Value) -> Song {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn requests_are_signed_over_sorted_parameters_without_format() {
        let params = vec![
            ("token", "TOK".to_string()),
            ("method", "auth.getSession".to_string()),
            ("api_key", "KEY".to_string()),
            ("format", "json".to_string()),
        ];
        // md5("api_keyKEYmethodauth.getSessiontokenTOK" + "SECRET")
        assert_eq!(signature(&params, "SECRET"), "0bf279e021f3a81b4553dd7e76cf72ad");
    }

    #[test]
    fn a_scrobble_names_the_main_artist_not_the_joined_credit() {
        let s = song(serde_json::json!({
            "id": "1", "title": "Song", "artist": "A feat. B", "album": "LP", "duration": 200,
            "artists": [{ "id": "a", "name": "A" }, { "id": "b", "name": "B" }]
        }));
        let p = track_params(&s, "[0]");
        assert!(p.contains(&("artist[0]", "A".to_string())));
        assert!(p.contains(&("track[0]", "Song".to_string())));
        assert!(p.contains(&("album[0]", "LP".to_string())));
        assert!(p.contains(&("duration[0]", "200".to_string())));
        let p = track_params(
            &song(serde_json::json!({ "id": "1", "title": "Song", "artist": "C" })),
            "",
        );
        assert_eq!(p, vec![("artist", "C".to_string()), ("track", "Song".to_string())]);
    }

    #[test]
    fn outages_are_retried_a_revoked_session_waits_and_bad_requests_are_dropped() {
        assert!(matches!(classify(16, String::new()), Failure::Retry(_)));
        assert!(matches!(classify(29, String::new()), Failure::Retry(_)));
        assert!(matches!(classify(9, String::new()), Failure::Session(_)));
        assert!(matches!(classify(6, String::new()), Failure::Drop(_)));
    }
}
