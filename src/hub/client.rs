use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use super::protocol::{self, PROTOCOL};
use crate::player::{Command, Snapshot, Status};
use crate::subsonic::{Song, Subsonic};

/// Who this player is on the hub.
#[derive(Debug, Clone)]
pub struct HubConfig {
    pub url: String,
    pub token: String,
    pub device_id: String,
    pub name: String,
    /// The "music server + account" hashes (see `server_keys`).
    pub server_keys: Vec<String>,
}

/// Messages to the hub.
type Out = mpsc::UnboundedSender<Value>;

/// While playing, the state is re-sent this often so other devices' clocks stay right.
const HEARTBEAT: Duration = Duration::from_secs(5);
const BACKOFF: [u64; 5] = [1, 2, 5, 10, 30];
/// After the hub refused the token or the protocol version, wait this long before trying again.
const REFUSED_WAIT: Duration = Duration::from_secs(300);
const COVER_SIZE: u32 = 400;
const COVER_MAX_BYTES: usize = 700_000;

/// Keeps a connection to the hub as a player, reconnecting with backoff, for as long as it runs.
pub async fn run(
    config: HubConfig,
    client: Subsonic,
    commands: mpsc::UnboundedSender<Command>,
    status: watch::Receiver<Status>,
) {
    let Some(url) = protocol::ws_url(&config.url) else {
        error!("not a valid hub address: {}", config.url);
        return;
    };
    let mut attempt = 0;
    loop {
        match session(&config, url.as_str(), &client, &commands, status.clone()).await {
            Ok(Some(code @ (4004 | 4005))) => {
                let why = if code == 4005 {
                    "refused the token"
                } else {
                    "speaks another protocol version"
                };
                error!("the hub {why}; trying again in {} minutes", REFUSED_WAIT.as_secs() / 60);
                tokio::time::sleep(REFUSED_WAIT).await;
                attempt = 0;
                continue;
            }
            Ok(code) => info!(
                "disconnected from the hub ({})",
                code.map_or("no close code".into(), |c| c.to_string())
            ),
            Err(e) => warn!("hub connection: {e:#}"),
        }
        let wait = BACKOFF[attempt.min(BACKOFF.len() - 1)];
        attempt += 1;
        tokio::time::sleep(Duration::from_secs(wait)).await;
    }
}

/// One connection, from hello to close. Returns the close code when the hub closed it.
async fn session(
    config: &HubConfig,
    url: &str,
    client: &Subsonic,
    commands: &mpsc::UnboundedSender<Command>,
    mut status: watch::Receiver<Status>,
) -> Result<Option<u16>> {
    let (ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .context("could not reach the hub")?;
    let (mut sink, mut stream) = ws.split();
    let (out, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if sink.send(Message::text(msg.to_string())).await.is_err() {
                break;
            }
        }
    });
    let _ = out.send(json!({
        "t": "hello", "v": PROTOCOL, "token": config.token, "deviceId": config.device_id,
        "name": config.name, "kind": "desktop", "server": config.server_keys,
    }));

    let ctx = Session {
        config: config.clone(),
        client: client.clone(),
        commands: commands.clone(),
        out: out.clone(),
        replacements: Arc::default(),
    };
    let mut reporter = Reporter::default();
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    let mut welcomed = false;
    let close_code = loop {
        tokio::select! {
            frame = stream.next() => {
                let Some(frame) = frame else { break None };
                match frame? {
                    Message::Text(text) => {
                        let Ok(msg) = serde_json::from_str::<Value>(&text) else { continue };
                        if msg["t"] == "welcome" {
                            info!("joined the hub as \"{}\"", config.name);
                            welcomed = true;
                            reporter = Reporter::default(); // a (re)joined hub has nothing from us yet
                            reporter.report(&status.borrow(), &ctx, true);
                        } else {
                            ctx.handle(&msg, &status);
                        }
                    }
                    Message::Close(frame) => break frame.map(|f| f.code.into()),
                    _ => {}
                }
            }
            changed = status.changed(), if welcomed => match changed {
                Ok(()) => {
                    let s = status.borrow_and_update().clone();
                    reporter.report(&s, &ctx, false);
                }
                Err(_) => break None, // the player stopped
            },
            _ = heartbeat.tick(), if welcomed => {
                let s = status.borrow().clone();
                if s.playing { let _ = out.send(protocol::state(&s)); }
            }
        }
    };
    writer.abort();
    Ok(close_code)
}

/// What message handling needs.
struct Session {
    config: HubConfig,
    client: Subsonic,
    commands: mpsc::UnboundedSender<Command>,
    out: Out,
    /// Bumped by every "play this" (replace) request: a lookup that finishes after a newer one is dropped.
    replacements: Arc<AtomicU64>,
}

impl Session {
    fn send(&self, msg: Value) {
        let _ = self.out.send(msg);
    }

    fn command(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    fn handle(&self, msg: &Value, status: &watch::Receiver<Status>) {
        match msg["t"].as_str().unwrap_or_default() {
            "command" => self.apply(msg["action"].as_str().unwrap_or_default(), &msg["args"]),
            "yield" => self.yield_to(msg["to"].as_str().unwrap_or_default().to_string()),
            "takeover" => self.take_over(msg),
            "queue-changed" => {
                // Another device saved a new shared queue. While idle, follow it so a later "play"
                // here plays what the others see; while playing, this device's queue stays.
                if !status.borrow().playing {
                    self.follow_saved_queue();
                }
            }
            _ => {} // state, devices, settings...: not needed by a headless player (yet)
        }
    }

    /// A command from another device (protocol `command` actions).
    fn apply(&self, action: &str, args: &Value) {
        let num = |key: &str| args[key].as_f64();
        match action {
            "play" => self.command(Command::Play),
            "pause" => self.command(Command::Pause),
            "toggle" => self.command(Command::Toggle),
            "next" => self.command(Command::Next),
            "prev" => self.command(Command::Previous),
            "seek" => {
                if let Some(sec) = num("positionSec").filter(|s| s.is_finite() && *s >= 0.0) {
                    self.command(Command::Seek(Duration::from_secs_f64(sec)));
                }
            }
            "volume" => {
                if let Some(v) = num("volume").filter(|v| v.is_finite()) {
                    self.command(Command::Volume(v.clamp(0.0, 1.0) as f32));
                }
            }
            "shuffle" => self.command(Command::SetShuffle(args["value"] == true)),
            "repeat" => {
                if let Some(repeat) = args["value"].as_str().and_then(protocol::parse_repeat) {
                    self.command(Command::SetRepeat(repeat));
                }
            }
            "shuffleQueue" => self.command(Command::ShuffleAll),
            "shuffleUpcomingQueue" => self.command(Command::ShuffleUpcoming),
            "playIndex" => {
                if let Some(index) = args["index"].as_u64() {
                    let track_id = args["trackId"].as_str().map(str::to_string);
                    self.command(Command::PlayIndex {
                        index: index as usize,
                        track_id,
                    });
                }
            }
            "playItem" => self.play_item(args),
            other => info!("ignoring the command \"{other}\""),
        }
    }

    /// "Play / play next / add this album, playlist or song" from a remote.
    fn play_item(&self, args: &Value) {
        let kind = args["kind"].as_str().unwrap_or_default().to_string();
        let Some(id) = args["id"].as_str().map(str::to_string) else {
            return;
        };
        let mode = args["mode"].as_str().unwrap_or("play").to_string();
        let index = args["index"].as_u64().unwrap_or(0) as usize;
        let replaces = mode != "next" && mode != "add";
        let generation = if replaces {
            self.replacements.fetch_add(1, Ordering::SeqCst) + 1
        } else {
            0
        };
        let (client, commands, replacements) = (self.client.clone(), self.commands.clone(), self.replacements.clone());
        tokio::spawn(async move {
            let songs = match songs_of(&client, &kind, &id).await {
                Ok(songs) => songs,
                Err(e) => return warn!("could not look up {kind} {id}: {e:#}"),
            };
            if replaces && replacements.load(Ordering::SeqCst) != generation {
                return; // a newer "play this" came in meanwhile
            }
            if songs.is_empty() {
                return;
            }
            let command = match mode.as_str() {
                "next" => Command::PlayNext(songs),
                "add" => Command::Enqueue(songs),
                _ => {
                    let start = if index < songs.len() { index } else { 0 };
                    Command::Load {
                        songs,
                        start,
                        position: Duration::ZERO,
                        play: true,
                    }
                }
            };
            let _ = commands.send(command);
        });
    }

    /// The hub hands playback to another device: pause, save the queue on the server, confirm.
    fn yield_to(&self, to: String) {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command(Command::Yield(reply_tx));
        let (client, out) = (self.client.clone(), self.out.clone());
        tokio::spawn(async move {
            let saved = match reply_rx.await {
                Ok(snapshot) => save_queue(&client, &snapshot).await,
                Err(_) => Err(anyhow::anyhow!("the player stopped")),
            };
            let reply = match saved {
                Ok(()) => json!({ "t": "yielded", "to": to, "ok": true }),
                Err(e) => {
                    error!("could not save the queue for the handoff: {e:#}");
                    json!({ "t": "yielded", "to": to, "ok": false, "reason": "queue_save_failed" })
                }
            };
            let _ = out.send(reply);
        });
    }

    /// The hub hands playback to this device: load the saved queue and play from where the other
    /// device was (the hub's position is more exact than the saved one when it is the same track).
    fn take_over(&self, msg: &Value) {
        let source_keys: Vec<String> = msg["server"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|k| k.as_str().map(str::to_string))
            .collect();
        if !source_keys.is_empty() && !source_keys.iter().any(|k| self.config.server_keys.contains(k)) {
            self.send(json!({ "t": "takeover-failed", "reason": "server_mismatch" }));
            return;
        }
        let state = msg["state"].clone();
        let age = Duration::from_millis(msg["ageMs"].as_u64().unwrap_or(0));
        let fresh = msg["fresh"] == true;
        let (client, commands, out) = (self.client.clone(), self.commands.clone(), self.out.clone());
        tokio::spawn(async move {
            match client.play_queue().await {
                Ok(saved) if !saved.songs.is_empty() => {
                    let start = saved
                        .current
                        .as_ref()
                        .and_then(|c| saved.songs.iter().position(|s| &s.id == c))
                        .unwrap_or(0);
                    let position = if fresh {
                        Duration::ZERO
                    } else {
                        takeover_position(&state, age, &saved.songs[start]).unwrap_or(saved.position)
                    };
                    let _ = commands.send(Command::Load {
                        songs: saved.songs,
                        start,
                        position,
                        play: true,
                    });
                }
                Ok(_) => {
                    let _ = out.send(json!({ "t": "takeover-failed", "reason": "queue_unavailable" }));
                }
                Err(e) => {
                    error!("could not load the saved queue: {e:#}");
                    let _ = out.send(json!({ "t": "takeover-failed", "reason": "queue_unavailable" }));
                }
            }
        });
    }

    /// Replaces the idle queue with the one saved on the server, without playing.
    fn follow_saved_queue(&self) {
        let (client, commands) = (self.client.clone(), self.commands.clone());
        tokio::spawn(async move {
            if let Ok(saved) = client.play_queue().await
                && !saved.songs.is_empty()
            {
                let start = saved
                    .current
                    .as_ref()
                    .and_then(|c| saved.songs.iter().position(|s| &s.id == c))
                    .unwrap_or(0);
                let _ = commands.send(Command::Load {
                    songs: saved.songs,
                    start,
                    position: saved.position,
                    play: false,
                });
            }
        });
    }
}

/// Where to resume a taken-over track: the hub's reported position, advanced by how long ago it was
/// reported if it was playing, when the hub's state is about this very track. None to use the saved one.
fn takeover_position(state: &Value, age: Duration, song: &Song) -> Option<Duration> {
    if state["trackId"].as_str() != Some(song.id.as_str()) {
        return None;
    }
    let mut sec = state["positionSec"].as_f64().filter(|s| s.is_finite() && *s >= 0.0)?;
    if state["playing"] == true {
        sec += age.as_secs_f64();
    }
    let end = f64::from(song.duration.unwrap_or(0)) - 0.5;
    Some(Duration::from_secs_f64(if end > 0.0 { sec.min(end) } else { sec }))
}

async fn songs_of(client: &Subsonic, kind: &str, id: &str) -> Result<Vec<Song>> {
    match kind {
        "album" => client.album_songs(id).await,
        "playlist" => client.playlist_songs(id).await,
        "song" => Ok(vec![client.song(id).await?]),
        other => Err(anyhow::anyhow!("unknown item kind \"{other}\"")),
    }
}

async fn save_queue(client: &Subsonic, snapshot: &Snapshot) -> Result<()> {
    let ids: Vec<String> = snapshot.songs.iter().map(|s| s.id.clone()).collect();
    client
        .save_play_queue(
            &ids,
            snapshot.current.as_ref().map(|s| s.id.as_str()),
            snapshot.position,
        )
        .await
}

/// Sends state, queue window and cover when they change.
#[derive(Default)]
struct Reporter {
    state_key: String,
    queue_key: String,
    cover_sent_for: String,
    position: Duration,
}

impl Reporter {
    fn report(&mut self, s: &Status, ctx: &Session, force: bool) {
        let track = s.song.as_ref().map(|x| x.id.as_str()).unwrap_or("");
        let key = format!("{track}|{}|{}|{}|{:?}", s.playing, s.volume, s.shuffle, s.repeat);
        // A position jump that isn't playback moving on is a seek: report it.
        let seeked = s.position + Duration::from_millis(1500) < self.position
            || s.position > self.position + Duration::from_secs(3);
        self.position = s.position;
        if force || key != self.state_key || seeked {
            self.state_key = key;
            ctx.send(protocol::state(s));
        }
        let queue_key = format!(
            "{}|{}",
            s.index,
            s.queue.iter().map(|x| x.id.as_str()).collect::<Vec<_>>().join(",")
        );
        if force || queue_key != self.queue_key {
            self.queue_key = queue_key;
            ctx.send(protocol::queue_window(&s.queue, s.index));
        }
        if let Some(song) = s.song.as_ref().filter(|_| s.playing)
            && (force || song.id != self.cover_sent_for)
        {
            self.cover_sent_for = song.id.clone();
            send_cover(song.clone(), ctx.client.clone(), ctx.out.clone());
        }
    }
}

/// Uploads the playing track's cover, so the web remote and REST clients can show it.
fn send_cover(song: Song, client: Subsonic, out: Out) {
    let Some(cover) = song.cover_art.clone() else { return };
    tokio::spawn(async move {
        if let Ok((bytes, mime)) = client.cover_art(&cover, COVER_SIZE).await
            && bytes.len() <= COVER_MAX_BYTES
            && matches!(mime.as_str(), "image/jpeg" | "image/png" | "image/webp")
        {
            let data = base64::engine::general_purpose::STANDARD.encode(bytes);
            let _ = out.send(json!({ "t": "cover", "trackId": song.id, "mime": mime, "data": data }));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: &str, duration: u32) -> Song {
        serde_json::from_value(json!({ "id": id, "duration": duration })).unwrap()
    }

    #[test]
    fn a_takeover_resumes_where_the_other_device_was_moving_on_by_the_time_since() {
        let state = json!({ "trackId": "s1", "positionSec": 60.0, "playing": true });
        let pos = takeover_position(&state, Duration::from_secs(2), &song("s1", 300)).unwrap();
        assert_eq!(pos, Duration::from_secs(62));
    }

    #[test]
    fn a_paused_device_resumes_exactly_where_it_was() {
        let state = json!({ "trackId": "s1", "positionSec": 60.0, "playing": false });
        assert_eq!(
            takeover_position(&state, Duration::from_secs(9), &song("s1", 300)),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn another_track_uses_the_position_saved_on_the_server() {
        let state = json!({ "trackId": "other", "positionSec": 60.0, "playing": true });
        assert_eq!(takeover_position(&state, Duration::ZERO, &song("s1", 300)), None);
    }

    #[test]
    fn a_takeover_never_starts_past_the_end() {
        let state = json!({ "trackId": "s1", "positionSec": 299.9, "playing": true });
        let pos = takeover_position(&state, Duration::from_secs(10), &song("s1", 300)).unwrap();
        assert!(pos <= Duration::from_secs_f64(299.5));
    }
}
