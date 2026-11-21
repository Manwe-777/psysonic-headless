use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

use super::queue::{Queue, Repeat};
use crate::audio::{Engine, EngineEvent, LoadedTrack, replay_gain_factor};
use crate::config::ReplayGainMode;
use crate::subsonic::{Song, Subsonic};

/// What the controller can be asked to do.
#[derive(Debug, Clone)]
pub enum Command {
    /// Replace the queue and play from `start`.
    Load {
        songs: Vec<Song>,
        start: usize,
    },
    Toggle,
    Next,
    Previous,
    Seek(Duration),
    /// Volume from 0.0 to 1.0.
    Volume(f32),
    SetRepeat(Repeat),
}

/// What plays, for the CLI and the hub.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub song: Option<Song>,
    pub index: usize,
    pub total: usize,
    pub playing: bool,
    pub position: Duration,
    pub volume: f32,
    pub repeat: Repeat,
    /// The last playback problem, if the latest attempt failed.
    pub error: Option<String>,
}

/// Why a track is being loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    /// Play it now.
    Start,
    /// Queue it after the current track.
    Next,
}

struct Loaded {
    generation: u64,
    index: usize,
    purpose: Purpose,
    result: Result<LoadedTrack>,
}

/// Before this point "previous" restarts the current track instead of going back (like Psysonic).
const RESTART_THRESHOLD: Duration = Duration::from_secs(3);
const STATUS_TICK: Duration = Duration::from_secs(1);

/// Plays the queue through the engine: loads the current track, preloads the next one while it
/// plays and queues it for a gapless change, and follows commands.
pub struct Controller {
    client: Subsonic,
    engine: Arc<Engine>,
    replay_gain: ReplayGainMode,
    queue: Queue,
    /// Bumped whenever the queue position changes by request; loads started before are dropped.
    generation: u64,
    next_token: u64,
    /// The token of the track playing.
    current: Option<u64>,
    /// A track already queued in the engine behind the current one: (token, queue position).
    queued: Option<(u64, usize)>,
    /// A preloaded track that can't follow seamlessly (other sample rate): played when the current ends.
    pending: Option<(LoadedTrack, usize)>,
    error: Option<String>,
    loaded_tx: mpsc::UnboundedSender<Loaded>,
    status_tx: watch::Sender<Status>,
}

impl Controller {
    /// Starts the controller; returns the command sender and the status receiver.
    pub fn spawn(
        client: Subsonic,
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
            engine: Arc::new(Engine::new(audio_device, volume, event_tx)),
            replay_gain,
            queue: Queue::default(),
            generation: 0,
            next_token: 0,
            current: None,
            queued: None,
            pending: None,
            error: None,
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
        let mut tick = tokio::time::interval(STATUS_TICK);
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => self.handle(command),
                    None => break, // everyone hung up
                },
                Some(EngineEvent::Ended { token }) = events.recv() => self.track_ended(token),
                Some(done) = loaded.recv() => self.track_loaded(done),
                _ = tick.tick() => {}
            }
            self.publish();
        }
        self.engine.stop();
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Load { songs, start } => {
                self.queue = Queue::new(songs, start);
                self.start_at(self.queue.index());
            }
            Command::Toggle => {
                if self.engine.is_paused() || self.current.is_none() {
                    self.resume();
                } else {
                    self.engine.pause();
                }
            }
            Command::Next => match self.queue.next_index() {
                Some(index) => self.start_at(index),
                None => self.stop(),
            },
            Command::Previous => {
                if self.engine.position() > RESTART_THRESHOLD {
                    self.seek(Duration::ZERO);
                } else {
                    self.start_at(self.queue.previous_index());
                }
            }
            Command::Seek(to) => self.seek(to),
            Command::Volume(v) => self.engine.set_volume(v),
            Command::SetRepeat(repeat) => {
                self.queue.repeat = repeat;
                // What follows the current track may have changed: preload it again.
                self.engine_drop_queued();
                self.preload_after(self.queue.index());
            }
        }
    }

    fn resume(&mut self) {
        if self.current.is_some() {
            self.engine.resume();
        } else if self.queue.current().is_some() {
            self.start_at(self.queue.index());
        }
    }

    fn seek(&mut self, to: Duration) {
        if let Err(e) = self.engine.seek(to) {
            warn!("{e}");
        }
    }

    fn stop(&mut self) {
        self.generation += 1;
        self.engine.stop();
        self.current = None;
        self.queued = None;
        self.pending = None;
    }

    /// Plays the queue position `index` now (from its start).
    fn start_at(&mut self, index: usize) {
        self.stop();
        self.queue.jump(index);
        self.load(index, Purpose::Start);
    }

    /// Forgets the track queued behind the current one. The engine keeps playing the current track;
    /// a queued track already appended can't be taken back, so it is only forgotten when not appended.
    fn engine_drop_queued(&mut self) {
        self.pending = None;
        if self.queued.is_none() {
            self.generation += 1;
        }
    }

    fn preload_after(&mut self, index: usize) {
        if self.queued.is_some() || self.pending.is_some() {
            return;
        }
        if let Some(next) = self.queue.after(index) {
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
        let tx = self.loaded_tx.clone();
        tokio::spawn(async move {
            let result = async {
                let bytes = client.fetch(&song.id).await?;
                let hint = song.suffix.clone();
                tokio::task::spawn_blocking(move || LoadedTrack::decode(token, bytes, hint.as_deref(), gain)).await?
            }
            .await;
            let _ = tx.send(Loaded {
                generation,
                index,
                purpose,
                result,
            });
        });
    }

    fn track_loaded(&mut self, done: Loaded) {
        if done.generation != self.generation {
            return; // the queue moved on while it loaded
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
            Purpose::Start => {
                let token = track.token;
                if let Err(e) = self.engine.play_now(track) {
                    error!("{e:#}");
                    self.error = Some(format!("{e:#}"));
                    return;
                }
                self.error = None;
                self.current = Some(token);
                self.announce();
                self.preload_after(done.index);
            }
            Purpose::Next => {
                let token = track.token;
                match self.engine.append(track) {
                    None => self.queued = Some((token, done.index)),
                    Some(track) => self.pending = Some((track, done.index)),
                }
            }
        }
    }

    fn track_ended(&mut self, token: u64) {
        if self.current != Some(token) {
            return;
        }
        if let Some((next, index)) = self.queued.take() {
            // Already playing, without a gap.
            self.current = Some(next);
            self.queue.jump(index);
            self.announce();
            self.preload_after(index);
        } else if let Some((track, index)) = self.pending.take() {
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
            self.start_at(index); // the next track wasn't ready in time
        } else {
            info!("end of the queue");
            self.current = None;
        }
    }

    fn announce(&self) {
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
        let playing = self.current.is_some() && !self.engine.is_paused();
        self.status_tx.send_replace(Status {
            song: self.current.and(self.queue.current().cloned()),
            index: self.queue.index(),
            total: self.queue.songs().len(),
            playing,
            position: self.engine.position(),
            volume: self.engine.volume(),
            repeat: self.queue.repeat,
            error: self.error.clone(),
        });
    }
}
