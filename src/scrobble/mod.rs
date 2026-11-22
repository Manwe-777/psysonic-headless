//! Scrobbling, like Psysonic: the song playing is reported as "now playing", and a play counts once
//! the set share of it has played (50% by default). Plays go to the server (play counts, recently
//! played) and, when signed in, to Last.fm. A Last.fm play that can't be sent is kept and retried.

pub mod lastfm;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::player::Status;
use crate::subsonic::{Song, Subsonic};
pub use lastfm::LastFm;

/// The share of a track that has to play, as Psysonic allows it (percent).
pub const PERCENT_RANGE: std::ops::RangeInclusive<i64> = 25..=90;
/// Last.fm takes no plays older than this.
const MAX_AGE: Duration = Duration::from_secs(14 * 24 * 3600);
const MAX_OWED: usize = 500;
const RETRY_EVERY: Duration = Duration::from_secs(60);

/// What to report.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    NowPlaying(Song),
    /// A play that counts, started at `at` (Unix milliseconds).
    Played {
        song: Song,
        at: i64,
    },
}

/// Turns the player's status into scrobble events.
pub struct Tracker {
    percent: u32,
    play: Option<Play>,
}

struct Play {
    id: u64,
    started_at: i64,
    announced: bool,
    counted: bool,
}

impl Tracker {
    pub fn new(percent: u8) -> Self {
        Self {
            percent: u32::from(percent),
            play: None,
        }
    }

    /// The events since the last look, at `now` (Unix milliseconds).
    pub fn observe(&mut self, s: &Status, now: i64) -> Vec<Event> {
        let Some(song) = &s.song else {
            self.play = None;
            return Vec::new();
        };
        let due = song
            .duration
            .filter(|d| *d > 0)
            .map(|d| Duration::from_secs(u64::from(d)) * self.percent / 100);
        if self.play.as_ref().map(|p| p.id) != Some(s.play) {
            self.play = Some(Play {
                id: s.play,
                // When it would have started from the beginning, as Psysonic counts it.
                started_at: now - s.position.as_millis() as i64,
                announced: false,
                // Taken over past the point where it counts: the device it came from counted it.
                counted: due.is_some_and(|d| s.play_start >= d),
            });
        }
        let play = self.play.as_mut().expect("set above");
        let mut out = Vec::new();
        if s.playing && !play.announced {
            play.announced = true;
            out.push(Event::NowPlaying(song.clone()));
        }
        if !play.counted && due.is_some_and(|d| s.position >= d) {
            play.counted = true;
            out.push(Event::Played {
                song: song.clone(),
                at: play.started_at,
            });
        }
        out
    }
}

/// A Last.fm play not sent yet.
struct Owed {
    song: Song,
    at: i64,
    attempts: u32,
    next_try: Instant,
}

/// Plays kept for Last.fm, oldest first.
#[derive(Default)]
struct OwedQueue(VecDeque<Owed>);

impl OwedQueue {
    fn keep(&mut self, song: Song, at: i64, attempts: u32, now: Instant) {
        self.0.push_back(Owed {
            song,
            at,
            attempts,
            next_try: now + backoff(attempts),
        });
        while self.0.len() > MAX_OWED {
            self.0.pop_front();
        }
    }

    /// The plays due for another try; the expired ones are dropped.
    fn take_due(&mut self, now: Instant, now_unix_ms: i64) -> Vec<Owed> {
        self.0.retain(|o| now_unix_ms - o.at < MAX_AGE.as_millis() as i64);
        let (due, wait): (Vec<_>, Vec<_>) = self.0.drain(..).partition(|o| o.next_try <= now);
        self.0 = wait.into();
        due
    }
}

/// 1, 2, 4 ... 60 minutes, like Psysonic.
fn backoff(attempts: u32) -> Duration {
    let minutes = 1u64 << attempts.saturating_sub(1).min(6);
    Duration::from_secs(60 * minutes.min(60))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Follows the player's status and reports its plays until the player stops.
pub fn spawn_scrobbler(server: Subsonic, lastfm: Option<LastFm>, percent: u8, mut status: watch::Receiver<Status>) {
    let owed = Arc::new(Mutex::new(OwedQueue::default()));
    tokio::spawn(async move {
        let mut tracker = Tracker::new(percent);
        let mut retry = tokio::time::interval(RETRY_EVERY);
        loop {
            tokio::select! {
                changed = status.changed() => {
                    if changed.is_err() {
                        return; // the player stopped
                    }
                    let events = tracker.observe(&status.borrow_and_update(), now_ms());
                    for event in events {
                        tokio::spawn(report(server.clone(), lastfm.clone(), owed.clone(), event));
                    }
                }
                _ = retry.tick() => {
                    if let Some(lastfm) = &lastfm {
                        let due = owed.lock().expect("owed").take_due(Instant::now(), now_ms());
                        if !due.is_empty() {
                            tokio::spawn(resend(lastfm.clone(), owed.clone(), due));
                        }
                    }
                }
            }
        }
    });
}

async fn report(server: Subsonic, lastfm: Option<LastFm>, owed: Arc<Mutex<OwedQueue>>, event: Event) {
    match event {
        Event::NowPlaying(song) => {
            if let Err(e) = server.scrobble(&song.id, None).await {
                debug!("now playing on the server: {e:#}");
            }
            if let Some(lastfm) = lastfm
                && let Err(e) = lastfm.now_playing(&song).await
            {
                debug!("now playing on Last.fm: {e}");
            }
        }
        Event::Played { song, at } => {
            match server.scrobble(&song.id, Some(at)).await {
                Ok(()) => debug!(song = %song.title, "scrobbled on the server"),
                Err(e) => warn!("the server didn't take the play of \"{}\": {e:#}", song.title),
            }
            if let Some(lastfm) = lastfm {
                send(&lastfm, &owed, song, at, 0).await;
            }
        }
    }
}

async fn resend(lastfm: LastFm, owed: Arc<Mutex<OwedQueue>>, due: Vec<Owed>) {
    for o in due {
        send(&lastfm, &owed, o.song, o.at, o.attempts).await;
    }
}

/// Sends one play to Last.fm; keeps it for later if Last.fm may take it then.
async fn send(lastfm: &LastFm, owed: &Mutex<OwedQueue>, song: Song, at: i64, attempts: u32) {
    use lastfm::Failure;
    match lastfm.scrobble(&song, at / 1000).await {
        Ok(()) => info!(song = %song.title, "scrobbled on Last.fm"),
        Err(Failure::Drop(m)) => warn!("Last.fm won't take the play of \"{}\": {m}", song.title),
        Err(e @ (Failure::Retry(_) | Failure::Session(_))) => {
            warn!("Last.fm play of \"{}\" kept for later: {e}", song.title);
            owed.lock().expect("owed").keep(song, at, attempts + 1, Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn song(id: &str, duration: u32) -> Song {
        serde_json::from_value(serde_json::json!({ "id": id, "title": id, "duration": duration })).unwrap()
    }

    fn status(song: &Song, play: u64, position: u64, playing: bool) -> Status {
        Status {
            song: Some(song.clone()),
            queue: Arc::new(vec![song.clone()]),
            playing,
            position: Duration::from_secs(position),
            play,
            ..Status::default()
        }
    }

    fn played(events: &[Event]) -> Vec<i64> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Played { at, .. } => Some(*at),
                Event::NowPlaying(_) => None,
            })
            .collect()
    }

    #[test]
    fn a_play_counts_once_half_of_it_has_played() {
        let s = song("a", 200);
        let mut t = Tracker::new(50);
        let first = t.observe(&status(&s, 1, 0, true), 1_000_000);
        assert_eq!(first, vec![Event::NowPlaying(s.clone())]);
        assert!(t.observe(&status(&s, 1, 99, true), 1_099_000).is_empty());
        // Started 100 s before this look.
        assert_eq!(
            played(&t.observe(&status(&s, 1, 100, true), 1_100_000)),
            vec![1_000_000]
        );
        assert!(
            t.observe(&status(&s, 1, 150, true), 1_150_000).is_empty(),
            "counted once"
        );
    }

    #[test]
    fn the_same_song_again_on_repeat_counts_again() {
        let s = song("a", 100);
        let mut t = Tracker::new(50);
        t.observe(&status(&s, 1, 0, true), 0);
        assert_eq!(played(&t.observe(&status(&s, 1, 60, true), 60_000)).len(), 1);
        t.observe(&status(&s, 2, 0, true), 100_000);
        assert_eq!(played(&t.observe(&status(&s, 2, 60, true), 160_000)), vec![100_000]);
    }

    #[test]
    fn a_paused_start_is_announced_once_it_plays() {
        let s = song("a", 100);
        let mut t = Tracker::new(50);
        assert!(t.observe(&status(&s, 1, 0, false), 0).is_empty());
        assert_eq!(
            t.observe(&status(&s, 1, 0, true), 5_000),
            vec![Event::NowPlaying(s.clone())]
        );
    }

    #[test]
    fn a_handoff_past_the_point_where_it_counts_is_not_counted_twice() {
        let s = song("a", 200);
        let mut t = Tracker::new(50);
        let mut taken = status(&s, 1, 120, true);
        taken.play_start = Duration::from_secs(120);
        t.observe(&taken, 0);
        assert!(played(&t.observe(&status(&s, 1, 190, true), 70_000)).is_empty());

        // Taken over before it: counted here.
        let mut t = Tracker::new(50);
        let mut taken = status(&s, 1, 60, true);
        taken.play_start = Duration::from_secs(60);
        t.observe(&taken, 1_000_000);
        assert_eq!(played(&t.observe(&status(&s, 1, 100, true), 1_040_000)), vec![940_000]);
    }

    #[test]
    fn skipping_before_the_point_does_not_count() {
        let (a, b) = (song("a", 100), song("b", 100));
        let mut t = Tracker::new(50);
        t.observe(&status(&a, 1, 0, true), 0);
        t.observe(&status(&a, 1, 30, true), 30_000);
        let events = t.observe(&status(&b, 2, 0, true), 31_000);
        assert_eq!(events, vec![Event::NowPlaying(b)]);
    }

    #[test]
    fn songs_without_a_length_never_count() {
        let s: Song = serde_json::from_value(serde_json::json!({ "id": "radio" })).unwrap();
        let mut t = Tracker::new(50);
        t.observe(&status(&s, 1, 0, true), 0);
        assert!(played(&t.observe(&status(&s, 1, 3600, true), 3_600_000)).is_empty());
    }

    #[test]
    fn kept_plays_wait_longer_each_time_and_expire_after_two_weeks() {
        assert_eq!(backoff(1), Duration::from_secs(60));
        assert_eq!(backoff(2), Duration::from_secs(120));
        assert_eq!(backoff(20), Duration::from_secs(3600));

        let now = Instant::now();
        let mut q = OwedQueue::default();
        let day = 24 * 3600 * 1000;
        q.keep(song("old", 100), 0, 1, now);
        q.keep(song("new", 100), 10 * day, 1, now);
        assert!(q.take_due(now, 15 * day).is_empty(), "not due yet");
        let due = q.take_due(now + Duration::from_secs(61), 15 * day);
        assert_eq!(due.iter().map(|o| o.song.id.as_str()).collect::<Vec<_>>(), vec!["new"]);
        assert!(q.0.is_empty());
    }

    #[test]
    fn at_most_500_plays_are_kept_dropping_the_oldest() {
        let now = Instant::now();
        let mut q = OwedQueue::default();
        for i in 0..=MAX_OWED {
            q.keep(song(&i.to_string(), 100), i as i64, 1, now);
        }
        assert_eq!(q.0.len(), MAX_OWED);
        assert_eq!(q.0.front().unwrap().song.id, "1");
    }
}
