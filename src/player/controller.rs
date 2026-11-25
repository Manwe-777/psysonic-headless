use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, error, info, warn};

use super::queue::{Queue, Repeat};
use crate::audio::{Engine, EngineEvent, LoadedTrack, replay_gain_factor};
use crate::cache::Cache;
use crate::config::ReplayGainMode;
use crate::subsonic::{Song, Subsonic};

/// What the controller can be asked to do.
#[derive(Debug)]
pub enum Command {
    /// Replace the queue and start at `start`, from `position`; paused unless `play`.
    Load {
        songs: Vec<Song>,
        start: usize,
        position: Duration,
        play: bool,
    },
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    Seek(Duration),
    /// Volume from 0.0 to 1.0.
    Volume(f32),
    SetRepeat(Repeat),
    /// Shuffle on (the upcoming songs are shuffled) or off.
    SetShuffle(bool),
    /// Shuffle the whole queue; the current song keeps playing.
    ShuffleAll,
    /// Shuffle the songs after the current one.
    ShuffleUpcoming,
    /// Play a queue position; ignored when `track_id` is given and another song is there now.
    PlayIndex {
        index: usize,
        track_id: Option<String>,
    },
    /// Add songs at the end of the queue.
    Enqueue(Vec<Song>),
    /// Insert songs right after the current one.
    PlayNext(Vec<Song>),
    /// Insert songs before a queue position.
    InsertAt {
        at: usize,
        songs: Vec<Song>,
    },
    /// Remove the entries a remote meant, as (position, song id) as it saw them. The song playing stays.
    Remove(Vec<(usize, String)>),
    /// Move the song at `from` so it ends up at `to`; ignored when another song is at `from` now.
    Move {
        from: usize,
        to: usize,
        track_id: String,
    },
    /// Move the songs at `indices` as one block into the gap before `gap`. With `expect`, only while
    /// those positions still hold these songs (the queue may have changed since the remote saw it).
    MoveBlock {
        indices: Vec<usize>,
        expect: Option<Vec<String>>,
        gap: usize,
    },
    /// Stop and empty the queue.
    Clear,
    /// Keep only the song playing.
    ClearExceptCurrent,
    /// Drop the songs after the one playing.
    DropUpcoming,
    /// Pause and report the queue and the exact position, for handing playback to another device.
    Yield(oneshot::Sender<Snapshot>),
}

/// The queue and position at one moment.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub songs: Arc<Vec<Song>>,
    pub current: Option<Song>,
    pub position: Duration,
}

/// What plays, for the CLI and the hub.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub song: Option<Song>,
    pub index: usize,
    pub queue: Arc<Vec<Song>>,
    pub playing: bool,
    pub position: Duration,
    pub volume: f32,
    pub repeat: Repeat,
    pub shuffle: bool,
    /// The last playback problem, if the latest attempt failed.
    pub error: Option<String>,
    /// Counts the plays: bumped whenever a track starts (also the same one again, on repeat).
    pub play: u64,
    /// Where the current play started (not zero after a handoff or a resume).
    pub play_start: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    /// Play it now, from this position, and start playing or stay paused.
    Start { position: Duration, play: bool },
    /// Follow the current track.
    Next,
}

struct Loaded {
    /// When the load started, to log how long a track took to start.
    started: std::time::Instant,
    generation: u64,
    index: usize,
    song_id: String,
    purpose: Purpose,
    result: Result<LoadedTrack>,
}

/// Before this point "previous" restarts the current track instead of going back (like Psysonic).
const RESTART_THRESHOLD: Duration = Duration::from_secs(3);
/// The next track is handed to the engine (for a gapless change) this long before the current ends.
/// Earlier, it only waits loaded, so queue edits up to then still decide what follows.
const APPEND_AHEAD: Duration = Duration::from_secs(10);
const TICK: Duration = Duration::from_millis(500);

/// Plays the queue through the engine: loads the current track, loads the next one early and hands
/// it to the engine shortly before the current ends (gapless), and follows commands.
pub struct Controller {
    client: Subsonic,
    cache: Option<Arc<Cache>>,
    engine: Arc<Engine>,
    replay_gain: ReplayGainMode,
    queue: Queue,
    shuffle: bool,
    /// Bumped whenever the queue moves by request; loads started before are dropped.
    generation: u64,
    next_token: u64,
    /// The token of the track playing.
    current: Option<u64>,
    /// The next track, loaded and waiting to be handed to the engine: (track, position, song id).
    preloaded: Option<(LoadedTrack, usize, String)>,
    /// The next track, already queued in the engine behind the current one: (token, position, song id).
    queued: Option<(u64, usize, String)>,
    /// A load of the next track is running.
    preloading: bool,
    error: Option<String>,
    play: u64,
    play_start: Duration,
    loaded_tx: mpsc::UnboundedSender<Loaded>,
    status_tx: watch::Sender<Status>,
}

impl Controller {
    /// Starts the controller; returns the command sender and the status receiver.
    pub fn spawn(
        client: Subsonic,
        cache: Option<Arc<Cache>>,
        audio_device: Option<String>,
        volume: f32,
        replay_gain: ReplayGainMode,
    ) -> (mpsc::UnboundedSender<Command>, watch::Receiver<Status>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (loaded_tx, loaded_rx) = mpsc::unbounded_channel();
        let (status_tx, status_rx) = watch::channel(Status {
            volume,
            ..Status::default()
        });
        let controller = Controller {
            client,
            cache,
            engine: Arc::new(Engine::new(audio_device, volume, event_tx)),
            replay_gain,
            queue: Queue::default(),
            shuffle: false,
            generation: 0,
            next_token: 0,
            current: None,
            preloaded: None,
            queued: None,
            preloading: false,
            error: None,
            play: 0,
            play_start: Duration::ZERO,
            loaded_tx,
            status_tx,
        };
        tokio::spawn(controller.run(cmd_rx, event_rx, loaded_rx));
        (cmd_tx, status_rx)
    }

    async fn run(
        mut self,
        mut commands: mpsc::UnboundedReceiver<Command>,
        mut events: mpsc::UnboundedReceiver<EngineEvent>,
        mut loaded: mpsc::UnboundedReceiver<Loaded>,
    ) {
        let mut tick = tokio::time::interval(TICK);
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => self.handle(command),
                    None => break, // everyone hung up
                },
                Some(EngineEvent::Ended { token }) = events.recv() => self.track_ended(token),
                Some(done) = loaded.recv() => self.track_loaded(done),
                _ = tick.tick() => self.append_when_due(),
            }
            self.publish();
        }
        self.engine.stop();
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Load {
                songs,
                start,
                position,
                play,
            } => {
                self.queue = Queue::new(songs, start);
                self.start_at(self.queue.index(), position, play);
            }
            Command::Play => self.resume(),
            Command::Pause => self.engine.pause(),
            Command::Toggle => {
                if self.engine.is_paused() || self.current.is_none() {
                    self.resume();
                } else {
                    self.engine.pause();
                }
            }
            Command::Next => match self.queue.next_index() {
                Some(index) => self.start_at(index, Duration::ZERO, true),
                None => self.stop(),
            },
            Command::Previous => {
                if self.engine.position() > RESTART_THRESHOLD {
                    self.seek(Duration::ZERO);
                } else {
                    self.start_at(self.queue.previous_index(), Duration::ZERO, true);
                }
            }
            Command::Seek(to) => self.seek(to),
            Command::Volume(v) => self.engine.set_volume(v),
            Command::SetRepeat(repeat) => {
                self.queue.repeat = repeat;
                self.next_changed();
            }
            Command::SetShuffle(on) => {
                self.shuffle = on;
                if on {
                    self.queue.shuffle_upcoming(&mut rand::rng());
                    self.next_changed();
                }
            }
            Command::ShuffleAll => {
                self.queue.shuffle_all(&mut rand::rng());
                self.next_changed();
            }
            Command::ShuffleUpcoming => {
                self.queue.shuffle_upcoming(&mut rand::rng());
                self.next_changed();
            }
            Command::PlayIndex { index, track_id } => {
                let matches = self
                    .queue
                    .get(index)
                    .is_some_and(|s| track_id.as_ref().is_none_or(|id| *id == s.id));
                if matches {
                    self.start_at(index, Duration::ZERO, true);
                }
            }
            Command::Enqueue(songs) => {
                let was_empty = self.queue.songs().is_empty();
                self.queue.append(songs);
                if was_empty {
                    self.start_at(0, Duration::ZERO, true);
                } else {
                    self.next_changed();
                }
            }
            Command::PlayNext(songs) => {
                let was_empty = self.queue.songs().is_empty();
                self.queue.insert_next(songs);
                if was_empty || self.current.is_none() {
                    self.start_at(self.queue.index(), Duration::ZERO, true);
                } else {
                    self.next_changed();
                }
            }
            Command::InsertAt { at, songs } => {
                let was_empty = self.queue.songs().is_empty();
                self.queue.insert_at(at, songs);
                if was_empty {
                    self.start_at(0, Duration::ZERO, true);
                } else {
                    self.next_changed();
                }
            }
            Command::Remove(positions) => {
                let picked = self.queue.pick(&positions);
                if self.queue.remove(&picked, self.current.is_some()) {
                    self.next_changed();
                }
            }
            Command::Move { from, to, track_id } => {
                if self.queue.get(from).is_some_and(|s| s.id == track_id) && self.queue.move_one(from, to) {
                    self.next_changed();
                }
            }
            Command::MoveBlock { indices, expect, gap } => {
                let still = expect.is_none_or(|ids| {
                    ids.len() == indices.len()
                        && indices
                            .iter()
                            .zip(&ids)
                            .all(|(i, id)| self.queue.get(*i).is_some_and(|s| s.id == *id))
                });
                if still && self.queue.move_block(&indices, gap) {
                    self.next_changed();
                }
            }
            Command::Clear => self.clear(),
            Command::ClearExceptCurrent => {
                if self.current.is_none() {
                    self.clear();
                } else {
                    self.queue.keep_only_current();
                    self.next_changed();
                }
            }
            Command::DropUpcoming => {
                if self.current.is_none() {
                    self.clear();
                } else {
                    self.queue.drop_upcoming();
                    self.next_changed();
                }
            }
            Command::Yield(reply) => {
                self.engine.pause();
                let _ = reply.send(Snapshot {
                    songs: Arc::new(self.queue.songs().to_vec()),
                    current: self.queue.current().cloned(),
                    position: self.engine.position(),
                });
            }
        }
    }

    fn resume(&mut self) {
        if self.current.is_some() {
            self.engine.resume();
        } else if self.queue.current().is_some() {
            self.start_at(self.queue.index(), Duration::ZERO, true);
        }
    }

    fn seek(&mut self, to: Duration) {
        if let Err(e) = self.engine.seek(to) {
            warn!("{e}");
        }
    }

    fn clear(&mut self) {
        self.stop();
        self.queue.clear();
    }

    fn stop(&mut self) {
        self.generation += 1;
        self.engine.stop();
        self.current = None;
        self.preloaded = None;
        self.queued = None;
        self.preloading = false;
    }

    /// Plays the queue position `index` from `position`.
    fn start_at(&mut self, index: usize, position: Duration, play: bool) {
        // The next track is often already loaded (or loading): use it instead of fetching it again.
        // Only from its start: a preloaded track sits at 0:00, and seeking it here would block.
        let ready = self
            .preloaded
            .take()
            .filter(|_| position.is_zero())
            .filter(|(_, i, id)| *i == index && self.queue.get(index).is_some_and(|s| s.id == *id || id.is_empty()));
        self.stop();
        self.queue.jump(index);
        match ready {
            Some((track, _, id)) => self.track_loaded(Loaded {
                started: std::time::Instant::now(),
                generation: self.generation,
                index,
                song_id: id,
                purpose: Purpose::Start { position, play },
                result: Ok(track),
            }),
            None => self.load(index, Purpose::Start { position, play }),
        }
    }

    /// The queue changed around the current song: what follows may be another song now.
    fn next_changed(&mut self) {
        let next = self.queue.after(self.queue.index());
        let next_id = next.and_then(|i| self.queue.get(i)).map(|s| s.id.clone());
        if let Some((_, index, id)) = &self.preloaded
            && (Some(*index) != next || Some(id) != next_id.as_ref())
        {
            self.preloaded = None;
        }
        if self.preloading {
            // A load for the old next song is running: drop its result, keep the current track.
            self.generation += 1;
            self.preloading = false;
        }
        if let Some((_, _, id)) = &self.queued {
            // Already handed to the engine (the current track is about to end); it stays.
            if next_id.as_ref() != Some(id) {
                warn!("the queue changed in the last seconds of a track: the queued next track still plays");
            }
            return;
        }
        if self.current.is_some() {
            self.preload_after(self.queue.index());
        }
    }

    fn preload_after(&mut self, index: usize) {
        if self.preloaded.is_some() || self.queued.is_some() || self.preloading {
            return;
        }
        if let Some(next) = self.queue.after(index) {
            self.preloading = true;
            self.load(next, Purpose::Next);
        }
    }

    /// Fetches and decodes a queue position in the background; the result arrives as `Loaded`.
    fn load(&mut self, index: usize, purpose: Purpose) {
        let Some(song) = self.queue.get(index).cloned() else {
            return;
        };
        self.next_token += 1;
        let token = self.next_token;
        let gain = replay_gain_factor(&song, self.replay_gain, self.queue.in_album(index));
        let generation = self.generation;
        let client = self.client.clone();
        let cache = self.cache.clone();
        let tx = self.loaded_tx.clone();
        let started = std::time::Instant::now();
        tokio::spawn(async move {
            let result = async {
                let start = match purpose {
                    Purpose::Start { position, .. } => position,
                    Purpose::Next => Duration::ZERO,
                };
                if let Some(cache) = &cache
                    && let Some(hit) = cache.lookup(&song.id)
                {
                    cache.touch(&hit.path);
                    let file = std::fs::File::open(&hit.path)?;
                    debug!(song = %song.title, "from the cache");
                    return tokio::task::spawn_blocking(move || {
                        LoadedTrack::decode_file(token, file, hit.hint.as_deref(), gain, start)
                    })
                    .await?;
                }
                let keep = cache
                    .as_ref()
                    .and_then(|c| c.stream_kind().map(|kind| (c.clone(), kind)));
                let (media, hint) = client.open_stream(&song, keep).await?;
                tokio::task::spawn_blocking(move || LoadedTrack::decode(token, media, hint, gain, start)).await?
            }
            .await;
            let _ = tx.send(Loaded {
                started,
                generation,
                index,
                song_id: song.id,
                purpose,
                result,
            });
        });
    }

    fn track_loaded(&mut self, done: Loaded) {
        if done.generation != self.generation {
            return; // the queue moved on while it loaded
        }
        if done.purpose == Purpose::Next {
            self.preloading = false;
        }
        let track = match done.result {
            Ok(track) => track,
            Err(e) => {
                let title = self.queue.get(done.index).map(|s| s.title.clone()).unwrap_or_default();
                error!("could not play \"{title}\": {e:#}");
                self.error = Some(format!("{e:#}"));
                return;
            }
        };
        match done.purpose {
            Purpose::Start { position, play } => {
                let token = track.token;
                if let Err(e) = self.engine.play_now(track) {
                    error!("{e:#}");
                    self.error = Some(format!("{e:#}"));
                    return;
                }
                let _ = position; // the track was opened there
                if !play {
                    self.engine.pause();
                }
                self.error = None;
                self.current = Some(token);
                info!(ms = done.started.elapsed().as_millis() as u64, "started");
                self.announce();
                self.preload_after(done.index);
            }
            Purpose::Next => {
                self.preloaded = Some((track, done.index, done.song_id));
                self.append_when_due();
            }
        }
    }

    /// Hands the preloaded next track to the engine once the current one is close to its end.
    fn append_when_due(&mut self) {
        let Some(duration) = self.queue.current().and_then(|s| s.duration) else {
            return;
        };
        let remaining = Duration::from_secs(u64::from(duration)).saturating_sub(self.engine.position());
        if self.current.is_none() || remaining > APPEND_AHEAD {
            return;
        }
        if let Some((track, index, id)) = self.preloaded.take() {
            let token = track.token;
            match self.engine.append(track) {
                None => self.queued = Some((token, index, id)),
                // Can't follow seamlessly (other sample rate): keep it, it starts when the current ends.
                Some(track) => self.preloaded = Some((track, index, id)),
            }
        }
    }

    fn track_ended(&mut self, token: u64) {
        if self.current != Some(token) {
            return;
        }
        if let Some((next, index, id)) = self.queued.take() {
            // Already playing, without a gap. A queue edit since may have moved it: find it by its id.
            let index = self.queue.find_after(index, &id);
            self.current = Some(next);
            self.queue.jump(index);
            self.announce();
            self.preload_after(index);
        } else if let Some((track, index, _)) = self.preloaded.take() {
            let token = track.token;
            self.queue.jump(index);
            match self.engine.play_now(track) {
                Ok(()) => {
                    self.current = Some(token);
                    self.announce();
                    self.preload_after(index);
                }
                Err(e) => {
                    error!("{e:#}");
                    self.current = None;
                }
            }
        } else if let Some(index) = self.queue.after(self.queue.index()) {
            self.start_at(index, Duration::ZERO, true); // the next track wasn't ready in time
        } else {
            info!("end of the queue");
            self.current = None;
        }
    }

    /// A track started playing (a new play, for scrobbling).
    fn announce(&mut self) {
        self.play += 1;
        self.play_start = self.engine.position();
        if let Some(song) = self.queue.current() {
            info!(
                position = self.queue.index() + 1,
                of = self.queue.songs().len(),
                "playing {} – {}",
                song.artist.as_deref().unwrap_or("?"),
                song.title
            );
        }
    }

    fn publish(&self) {
        let queue_changed = {
            let current = self.status_tx.borrow();
            current.queue.len() != self.queue.songs().len()
                || current.queue.iter().zip(self.queue.songs()).any(|(a, b)| a.id != b.id)
        };
        let queue = if queue_changed {
            Arc::new(self.queue.songs().to_vec())
        } else {
            self.status_tx.borrow().queue.clone()
        };
        self.status_tx.send_replace(Status {
            song: self.current.and(self.queue.current().cloned()),
            index: self.queue.index(),
            queue,
            playing: self.current.is_some() && !self.engine.is_paused(),
            position: self.engine.position(),
            volume: self.engine.volume(),
            repeat: self.queue.repeat,
            shuffle: self.shuffle,
            error: self.error.clone(),
            play: self.play,
            play_start: self.play_start,
        });
    }
}
