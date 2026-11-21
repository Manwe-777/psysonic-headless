//! Message shapes of the hub protocol v1 (see the hub's docs/PROTOCOL.md), built from player status.

use serde_json::{Value, json};
use url::Url;

use crate::player::{Repeat, Status};
use crate::subsonic::Song;

pub const PROTOCOL: u32 = 1;
/// How many tracks before the current one the queue window holds; the rest are upcoming.
const QUEUE_BEFORE: usize = 20;
/// The hub keeps at most this many tracks per queue window.
const QUEUE_MAX: usize = 300;

/// The hub's WebSocket address for a hub URL as users type it (`http://host:5533` → `ws://host:5533/ws`).
pub fn ws_url(input: &str) -> Option<Url> {
    let raw = input.trim();
    if raw.is_empty() {
        return None;
    }
    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let mut url = Url::parse(&with_scheme).ok()?;
    let scheme = match url.scheme() {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        _ => return None,
    };
    url.set_scheme(scheme).ok()?;
    if url.path().is_empty() || url.path() == "/" {
        url.set_path("/ws");
    }
    Some(url)
}

pub fn repeat_name(repeat: Repeat) -> &'static str {
    match repeat {
        Repeat::Off => "off",
        Repeat::All => "all",
        Repeat::One => "one",
    }
}

pub fn parse_repeat(value: &str) -> Option<Repeat> {
    match value {
        "off" => Some(Repeat::Off),
        "all" => Some(Repeat::All),
        "one" => Some(Repeat::One),
        _ => None,
    }
}

/// The `state` message for the current status.
pub fn state(status: &Status) -> Value {
    let song = status.song.as_ref();
    json!({
        "t": "state",
        "state": {
            "trackId": song.map(|s| &s.id),
            "title": song.map(|s| &s.title),
            "artist": song.and_then(|s| s.artist.as_ref()),
            "album": song.and_then(|s| s.album.as_ref()),
            "albumId": song.and_then(|s| s.album_id.as_ref()),
            "artistId": song.and_then(|s| s.artist_id.as_ref()),
            "coverArt": song.and_then(|s| s.cover_art.as_ref()),
            "durationSec": song.and_then(|s| s.duration),
            "positionSec": status.position.as_secs_f64(),
            "playing": status.playing,
            "volume": status.volume,
            "shuffle": status.shuffle,
            "repeat": repeat_name(status.repeat),
        }
    })
}

fn track(song: &Song) -> Value {
    json!({
        "id": song.id,
        "title": song.title,
        "artist": song.artist,
        "album": song.album,
        "albumId": song.album_id,
        "artistId": song.artist_id,
        "coverArt": song.cover_art,
        "durationSec": song.duration,
    })
}

/// The `queue` message: a window of the queue around the current track, with absolute positions.
pub fn queue_window(songs: &[Song], index: usize) -> Value {
    let total = songs.len();
    let current = if total == 0 { -1 } else { index.min(total - 1) as i64 };
    let offset = (current.max(0) as usize)
        .saturating_sub(QUEUE_BEFORE)
        .min(total.saturating_sub(QUEUE_MAX));
    let tracks: Vec<Value> = songs.iter().skip(offset).take(QUEUE_MAX).map(track).collect();
    json!({ "t": "queue", "queue": { "index": current, "offset": offset, "total": total, "tracks": tracks } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn songs(n: usize) -> Vec<Song> {
        (0..n)
            .map(|i| serde_json::from_value(json!({ "id": format!("s{i}") })).unwrap())
            .collect()
    }

    #[test]
    fn hub_addresses_are_turned_into_its_websocket() {
        assert_eq!(ws_url("192.168.1.2:5533").unwrap().as_str(), "ws://192.168.1.2:5533/ws");
        assert_eq!(
            ws_url("https://hub.example.com").unwrap().as_str(),
            "wss://hub.example.com/ws"
        );
        assert_eq!(
            ws_url("https://hub.example.com/custom").unwrap().as_str(),
            "wss://hub.example.com/custom"
        );
        assert!(ws_url("ftp://hub").is_none());
        assert!(ws_url("  ").is_none());
    }

    #[test]
    fn a_long_queue_sends_a_few_played_tracks_and_as_many_upcoming_ones_as_fit() {
        let q = queue_window(&songs(1000), 500)["queue"].clone();
        assert_eq!(q["offset"], 480);
        assert_eq!(q["tracks"].as_array().unwrap().len(), 300);
        assert_eq!(q["tracks"][20]["id"], "s500");
        assert_eq!(q["total"], 1000);
    }

    #[test]
    fn near_the_end_the_window_still_fills_up() {
        let q = queue_window(&songs(1000), 995)["queue"].clone();
        assert_eq!(q["tracks"].as_array().unwrap().len(), 300);
        assert_eq!(q["tracks"][299]["id"], "s999");
    }

    #[test]
    fn an_empty_queue_has_no_current_track() {
        let q = queue_window(&[], 0)["queue"].clone();
        assert_eq!(q["index"], -1);
        assert_eq!(q["tracks"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn state_reports_repeat_by_name() {
        let status = Status {
            repeat: Repeat::All,
            playing: true,
            ..Status::default()
        };
        let s = state(&status);
        assert_eq!(s["state"]["repeat"], "all");
        assert_eq!(s["state"]["playing"], true);
    }
}
