//! Fetches the next tracks of the queue into the cache ahead of time (the desktop fetches the next
//! five), within the size limit: the next track always, the ones after only while they fit.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::sync::watch;
use tracing::{debug, warn};

use super::{Cache, Kind, estimate};
use crate::player::{Repeat, Status};
use crate::subsonic::{Song, Subsonic, container_hint};

/// How many tracks after the current one are fetched.
pub const PREFETCH_AHEAD: usize = 5;
/// Wait for the queue to settle before fetching (a burst of edits plans once).
const SETTLE: Duration = Duration::from_secs(2);

/// Keeps the cache filled with the tracks coming up, following the player's status.
pub fn spawn_prefetch(cache: Arc<Cache>, client: Subsonic, mut status: watch::Receiver<Status>) {
    tokio::spawn(async move {
        let mut done_for: Vec<String> = Vec::new();
        loop {
            let plan = upcoming(&status.borrow_and_update());
            protect(&cache, &status.borrow());
            if ids(&plan) != done_for {
                tokio::time::sleep(SETTLE).await;
                if ids(&upcoming(&status.borrow())) == ids(&plan) {
                    fetch_plan(&cache, &client, &plan, &status).await;
                    done_for = ids(&plan);
                }
                continue; // look again: the queue moved while waiting or fetching
            }
            if status.changed().await.is_err() {
                return; // the player stopped
            }
        }
    });
}

/// The tracks after the current one, in the order they will play.
pub fn upcoming(status: &Status) -> Vec<Song> {
    let queue = &status.queue;
    if status.song.is_none() || queue.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for step in 1..=PREFETCH_AHEAD {
        let i = status.index + step;
        let song = match (queue.get(i), status.repeat) {
            (Some(song), _) => song,
            (None, Repeat::All) => &queue[i % queue.len()],
            (None, _) => break,
        };
        if Some(&song.id) == status.song.as_ref().map(|s| &s.id) {
            break; // wrapped around to the current one
        }
        out.push(song.clone());
    }
    out
}

fn ids(songs: &[Song]) -> Vec<String> {
    songs.iter().map(|s| s.id.clone()).collect()
}

fn protect(cache: &Cache, status: &Status) {
    let current = status.song.as_ref().map(|s| s.id.clone());
    let next = upcoming(status).first().map(|s| s.id.clone());
    cache.protect(current.into_iter().chain(next));
}

async fn fetch_plan(cache: &Arc<Cache>, client: &Subsonic, plan: &[Song], status: &watch::Receiver<Status>) {
    let kind = cache.prefetch_kind();
    let mut budget = 0u64;
    for (i, song) in plan.iter().enumerate() {
        budget += estimate(song, &kind, client.quality());
        if i > 0 && budget > cache.max_bytes() {
            break; // the next track always; the ones after while they fit
        }
        if cache.contains(&song.id) {
            continue;
        }
        if ids(&upcoming(&status.borrow())) != ids(plan) {
            return; // the queue moved: plan again
        }
        match fetch(cache, client, song, kind.clone()).await {
            Ok(()) => debug!(song = %song.title, "prefetched"),
            Err(e) => warn!("could not prefetch \"{}\": {e:#}", song.title),
        }
    }
}

/// Downloads one track into the cache.
async fn fetch(cache: &Arc<Cache>, client: &Subsonic, song: &Song, kind: Kind) -> Result<()> {
    let original = kind == Kind::Original;
    let mut response = client.download(&song.id, original).await?;
    let Some(len) = response.content_length().filter(|n| *n > 0) else {
        bail!("the server did not say the size");
    };
    if original && song.size.is_some_and(|size| size != len) {
        bail!(
            "the server sent {len} bytes, not the original's {} (a transcode?)",
            song.size.unwrap_or(0)
        );
    }
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let ext = if original {
        song.suffix.clone()
    } else {
        container_hint(content_type).map(str::to_string)
    };
    let mut writer = cache.writer(&song.id, kind, ext.as_deref()).await?;
    while let Some(chunk) = response.chunk().await? {
        writer.write(&chunk).await?;
    }
    writer.commit(len).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: &str) -> Song {
        serde_json::from_value(serde_json::json!({ "id": id })).unwrap()
    }

    fn status(ids: &[&str], index: usize, repeat: Repeat) -> Status {
        let queue: Vec<Song> = ids.iter().map(|id| song(id)).collect();
        Status {
            song: queue.get(index).cloned(),
            index,
            queue: Arc::new(queue),
            repeat,
            ..Status::default()
        }
    }

    #[test]
    fn the_next_five_tracks_are_planned_in_play_order() {
        let s = status(&["a", "b", "c", "d", "e", "f", "g", "h"], 1, Repeat::Off);
        assert_eq!(ids(&upcoming(&s)), ["c", "d", "e", "f", "g"]);
    }

    #[test]
    fn the_end_of_the_queue_ends_the_plan_unless_it_repeats() {
        let s = status(&["a", "b", "c"], 1, Repeat::Off);
        assert_eq!(ids(&upcoming(&s)), ["c"]);
        let s = status(&["a", "b", "c"], 1, Repeat::All);
        assert_eq!(ids(&upcoming(&s)), ["c", "a"], "wraps around, up to the current track");
    }

    #[test]
    fn nothing_playing_plans_nothing() {
        let mut s = status(&["a", "b"], 0, Repeat::Off);
        s.song = None;
        assert!(upcoming(&s).is_empty());
    }
}
