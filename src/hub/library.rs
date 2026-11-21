//! Library browsing for hub remotes, answered from the music server the same way the Psysonic
//! desktop does (`src/features/hub/hubLibrary.ts`): the hub has no server access of its own, so
//! it asks a player, and the player looks it up and answers.

use anyhow::{Result, bail};
use base64::Engine;
use serde_json::{Map, Value, json};

use crate::subsonic::Subsonic;

/// Albums per page (as the desktop).
pub const ALBUM_PAGE: usize = 40;
/// Artists per page (as the desktop).
pub const ARTIST_PAGE: usize = 200;
/// The largest image the hub accepts.
const ART_MAX_BYTES: usize = 250_000;
const ALBUM_SORTS: [&str; 7] = [
    "newest",
    "recent",
    "frequent",
    "random",
    "alphabeticalByName",
    "alphabeticalByArtist",
    "starred",
];

/// Answers one browse request.
pub async fn browse(client: &Subsonic, req: &Value) -> Result<Value> {
    let id = req["id"].as_str().unwrap_or_default();
    let offset = req["offset"].as_u64().unwrap_or(0) as usize;
    Ok(match req["kind"].as_str().unwrap_or_default() {
        "albums" => {
            let sort = req["sort"]
                .as_str()
                .filter(|s| ALBUM_SORTS.contains(s))
                .unwrap_or("newest");
            let (size, offset) = (ALBUM_PAGE.to_string(), offset.to_string());
            let mut params = vec![("type", sort), ("size", size.as_str()), ("offset", offset.as_str())];
            if sort == "random" {
                params.truncate(2); // random takes no offset
            }
            let list = client.raw("getAlbumList2.view", &params, "albumList2").await?;
            let albums = items(&list["album"], album_item);
            json!({ "more": albums.len() == ALBUM_PAGE, "albums": albums })
        }
        "artists" => {
            let index = client.raw("getArtists.view", &[], "artists").await?;
            let all: Vec<Value> = list(&index["index"])
                .iter()
                .flat_map(|i| items(&i["artist"], artist_item))
                .collect();
            let page: Vec<Value> = all.iter().skip(offset).take(ARTIST_PAGE).cloned().collect();
            json!({ "artists": page, "more": all.len() > offset + ARTIST_PAGE })
        }
        "search" => {
            let query = req["query"].as_str().unwrap_or_default();
            let found = client
                .raw(
                    "search3.view",
                    &[
                        ("query", query),
                        ("artistCount", "10"),
                        ("albumCount", "20"),
                        ("songCount", "40"),
                    ],
                    "searchResult3",
                )
                .await?;
            json!({
                "artists": items(&found["artist"], artist_item),
                "albums": items(&found["album"], album_item),
                "songs": items(&found["song"], song_item),
            })
        }
        "album" => {
            let album = client.raw("getAlbum.view", &[("id", id)], "album").await?;
            json!({ "item": album_item(&album), "songs": items(&album["song"], song_item) })
        }
        "artist" => {
            let artist = client.raw("getArtist.view", &[("id", id)], "artist").await?;
            json!({ "item": artist_item(&artist), "albums": items(&artist["album"], album_item) })
        }
        "playlists" => {
            let lists = client.raw("getPlaylists.view", &[], "playlists").await?;
            json!({ "playlists": items(&lists["playlist"], playlist_item) })
        }
        "playlist" => {
            let playlist = client.raw("getPlaylist.view", &[("id", id)], "playlist").await?;
            json!({ "item": playlist_item(&playlist), "songs": items(&playlist["entry"], song_item) })
        }
        other => bail!("unknown browse kind \"{other}\""),
    })
}

/// Artwork for a library page, as the hub's `art` message fields, or none.
pub async fn artwork(client: &Subsonic, cover_art: &str, size: u32) -> Option<(String, String)> {
    let (bytes, mime) = client.cover_art(cover_art, size).await.ok()?;
    let ok = bytes.len() <= ART_MAX_BYTES && matches!(mime.as_str(), "image/jpeg" | "image/png" | "image/webp");
    ok.then(|| (mime, base64::engine::general_purpose::STANDARD.encode(bytes)))
}

/// A list from the server: some servers send a single object where there is one entry.
fn list(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        Value::Object(_) => vec![v.clone()],
        _ => Vec::new(),
    }
}

fn items(v: &Value, item: fn(&Value) -> Value) -> Vec<Value> {
    list(v).iter().filter(|x| x["id"].is_string()).map(item).collect()
}

/// Copies the given fields that are present, under the names the hub uses.
fn pick(v: &Value, fields: &[(&str, &str)]) -> Value {
    let mut out = Map::new();
    for (from, to) in fields {
        if let Some(x) = v.get(*from).filter(|x| !x.is_null()) {
            out.insert((*to).to_string(), x.clone());
        }
    }
    Value::Object(out)
}

fn album_item(a: &Value) -> Value {
    pick(
        a,
        &[
            ("id", "id"),
            ("name", "name"),
            ("artist", "artist"),
            ("artistId", "artistId"),
            ("coverArt", "coverArt"),
            ("year", "year"),
            ("songCount", "songCount"),
        ],
    )
}

fn artist_item(a: &Value) -> Value {
    pick(
        a,
        &[
            ("id", "id"),
            ("name", "name"),
            ("coverArt", "coverArt"),
            ("albumCount", "albumCount"),
        ],
    )
}

fn playlist_item(p: &Value) -> Value {
    pick(
        p,
        &[
            ("id", "id"),
            ("name", "name"),
            ("coverArt", "coverArt"),
            ("songCount", "songCount"),
            ("duration", "durationSec"),
        ],
    )
}

fn song_item(s: &Value) -> Value {
    pick(
        s,
        &[
            ("id", "id"),
            ("title", "title"),
            ("artist", "artist"),
            ("artistId", "artistId"),
            ("album", "album"),
            ("albumId", "albumId"),
            ("coverArt", "coverArt"),
            ("duration", "durationSec"),
            ("track", "track"),
            ("discNumber", "disc"),
            ("year", "year"),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_song_is_sent_under_the_hub_names_with_only_what_the_server_gave() {
        let song = json!({ "id": "s1", "title": "Acid Hologram", "duration": 241, "discNumber": 1,
            "path": "/music/secret/path.flac", "bitRate": 900, "artist": null });
        assert_eq!(
            song_item(&song),
            json!({ "id": "s1", "title": "Acid Hologram", "durationSec": 241, "disc": 1 })
        );
    }

    #[test]
    fn a_single_entry_sent_as_an_object_still_counts_as_a_list() {
        assert_eq!(
            items(&json!({ "id": "a1", "name": "One" }), album_item),
            vec![json!({ "id": "a1", "name": "One" })]
        );
        assert!(items(&Value::Null, album_item).is_empty());
    }

    #[test]
    fn entries_without_an_id_are_left_out() {
        let albums = json!([{ "name": "no id" }, { "id": "a2", "name": "Two" }]);
        assert_eq!(items(&albums, album_item).len(), 1);
    }

    #[test]
    fn a_playlist_duration_is_sent_in_seconds_under_the_hub_name() {
        let p = json!({ "id": "p1", "name": "Mix", "duration": 3600, "songCount": 12, "owner": "manuel" });
        assert_eq!(
            playlist_item(&p),
            json!({ "id": "p1", "name": "Mix", "durationSec": 3600, "songCount": 12 })
        );
    }
}
