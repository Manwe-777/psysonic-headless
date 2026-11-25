use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use super::library;
use super::protocol::{self, PROTOCOL};
use super::queue_edits::{AddMode, Edit, EditReader};
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
/// A ping goes to the hub this often; the hub answers it (and sends its own every 30 s).
const KEEPALIVE: Duration = Duration::from_secs(20);
/// Nothing at all from the hub for this long means the connection is dead (after a sleep or a
/// network change it can look open while nothing gets through), so the player reconnects.
const SILENCE_LIMIT: Duration = Duration::from_secs(45);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const BACKOFF: [u64; 5] = [1, 2, 5, 10, 30];
/// After the hub refused the token or the protocol version, wait this long before trying again.
const REFUSED_WAIT: Duration = Duration::from_secs(300);
/// A queue change is saved on the server this long after the last one (edits come in bursts).
const SAVE_DELAY: Duration = Duration::from_millis(400);
/// Songs a remote asked to add are looked up this many at a time.
const LOOKUPS_AT_ONCE: usize = 8;
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
    // The queue known to be saved on the server (song ids), across reconnects.
    let saved_queue = SavedQueue::default();
    let mut attempt = 0;
    loop {
        match session(&config, url.as_str(), &client, &commands, status.clone(), &saved_queue).await {
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
    saved_queue: &SavedQueue,
) -> Result<Option<u16>> {
    let (ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url))
        .await
        .map_err(|_| anyhow!("the hub didn't answer within {} s", CONNECT_TIMEOUT.as_secs()))?
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

    // Queue edits from remotes are applied one after another, in the order they came in.
    let (edits, mut edits_rx) = mpsc::unbounded_channel::<Edit>();
    let (edited, mut edited_rx) = mpsc::unbounded_channel::<()>();
    let editor = {
        let (client, commands) = (client.clone(), commands.clone());
        tokio::spawn(async move {
            while let Some(edit) = edits_rx.recv().await {
                apply_edit(&client, &commands, edit).await;
                let _ = edited.send(());
            }
        })
    };
    let ctx = Session {
        config: config.clone(),
        client: client.clone(),
        commands: commands.clone(),
        out: out.clone(),
        replacements: Arc::default(),
        saved_queue: saved_queue.clone(),
        edits,
        reader: Mutex::default(),
    };
    let mut reporter = Reporter::default();
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    let mut keepalive = tokio::time::interval_at(Instant::now() + KEEPALIVE, KEEPALIVE);
    let mut last_heard = Instant::now();
    let mut welcomed = false;
    // When the queue is next saved (or, after an edit that changed nothing, only announced).
    let mut save_at: Option<Instant> = None;
    let mut edit_seen = false;
    let mut queue_seen = queue_key(&status.borrow().queue);
    let close_code = loop {
        tokio::select! {
            frame = stream.next() => {
                let Some(frame) = frame else { break None };
                let frame = frame?;
                last_heard = Instant::now();
                match frame {
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
                    let key = queue_key(&s.queue);
                    if key != queue_seen {
                        queue_seen = key;
                        save_at = Some(Instant::now() + SAVE_DELAY);
                    }
                }
                Err(_) => break None, // the player stopped
            },
            Some(()) = edited_rx.recv() => {
                edit_seen = true;
                save_at = Some(Instant::now() + SAVE_DELAY);
            }
            _ = tokio::time::sleep_until(save_at.unwrap_or_else(|| Instant::now() + KEEPALIVE)), if save_at.is_some() => {
                save_at = None;
                let s = status.borrow().clone();
                ctx.save_queue_change(&s, std::mem::take(&mut edit_seen));
            }
            _ = heartbeat.tick(), if welcomed => {
                let s = status.borrow().clone();
                if s.playing { let _ = out.send(protocol::state(&s)); }
            }
            _ = keepalive.tick() => {
                if last_heard.elapsed() > SILENCE_LIMIT {
                    writer.abort();
                    bail!("the hub stopped answering");
                }
                let _ = out.send(json!({ "t": "ping" }));
            }
        }
    };
    writer.abort();
    editor.abort();
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
    saved_queue: SavedQueue,
    edits: mpsc::UnboundedSender<Edit>,
    reader: Mutex<EditReader>,
}

/// The song ids of the queue known to be on the server: saved here, or pulled from it. A queue
/// that matches it is not saved again (a pulled queue saved straight back would bounce between devices).
#[derive(Clone, Default)]
struct SavedQueue(Arc<Mutex<String>>);

impl SavedQueue {
    fn set(&self, key: String) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = key;
    }

    fn get(&self) -> String {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

fn queue_key(songs: &[Song]) -> String {
    songs.iter().map(|s| s.id.as_str()).collect::<Vec<_>>().join(",")
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
            "command" => {
                let action = msg["action"].as_str().unwrap_or_default();
                let edit =
                    self.reader
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .read(action, &msg["args"], Instant::now());
                match edit {
                    Some(edit) => {
                        let _ = self.edits.send(edit);
                    }
                    None if action == "queueAdd" => {} // waiting for the add's other parts
                    None => self.apply(action, &msg["args"]),
                }
            }
            "yield" => self.yield_to(msg["to"].as_str().unwrap_or_default().to_string()),
            "takeover" => self.take_over(msg),
            "queue-changed" => {
                // Another device saved a new shared queue. While idle, follow it so a later "play"
                // here plays what the others see; while playing, this device's queue stays.
                if !status.borrow().playing {
                    self.follow_saved_queue();
                }
            }
            "browse" => self.answer_browse(msg["rid"].clone(), msg["req"].clone()),
            "art-request" => self.answer_art(msg),
            _ => {} // state, devices, settings...: not needed by a headless player (yet)
        }
    }

    /// The queue changed here. As the player, this device is the one that saves the shared queue:
    /// save it and tell the others (they pull it). Only while playing or after a remote's edit, so
    /// an idle device never overwrites the queue another one plays. An edit that changed nothing is
    /// still announced, so the remote drops what it showed and pulls the real queue.
    fn save_queue_change(&self, s: &Status, edited: bool) {
        let key = queue_key(&s.queue);
        if key == self.saved_queue.get() || !(s.playing || edited) {
            if edited {
                self.send(json!({ "t": "queue-changed" }));
            }
            return;
        }
        let snapshot = Snapshot {
            songs: s.queue.clone(),
            current: s.queue.get(s.index).cloned(),
            position: s.position,
        };
        let (client, out, saved_queue) = (self.client.clone(), self.out.clone(), self.saved_queue.clone());
        tokio::spawn(async move {
            match save_queue(&client, &snapshot).await {
                Ok(()) => {
                    saved_queue.set(key);
                    let _ = out.send(json!({ "t": "queue-changed" }));
                }
                Err(e) => warn!("could not save the queue: {e:#}"),
            }
        });
    }

    /// A remote browses the library through this player.
    fn answer_browse(&self, rid: Value, req: Value) {
        let (client, out) = (self.client.clone(), self.out.clone());
        tokio::spawn(async move {
            let reply = match library::browse(&client, &req).await {
                Ok(result) => json!({ "t": "browse-result", "rid": rid, "result": result }),
                Err(e) => {
                    warn!("library lookup failed: {e:#}");
                    json!({ "t": "browse-result", "rid": rid, "error": "lookup_failed" })
                }
            };
            let _ = out.send(reply);
        });
    }

    /// The hub wants a cover for a library page (an answer without data means there is none).
    fn answer_art(&self, msg: &Value) {
        let (client, out) = (self.client.clone(), self.out.clone());
        let rid = msg["rid"].clone();
        let cover = msg["coverArt"].as_str().unwrap_or_default().to_string();
        let size = msg["size"].as_u64().unwrap_or(160).clamp(32, 1200) as u32;
        tokio::spawn(async move {
            let art = if cover.is_empty() {
                None
            } else {
                library::artwork(&client, &cover, size).await
            };
            let reply = match art {
                Some((mime, data)) => json!({ "t": "art", "rid": rid, "mime": mime, "data": data }),
                None => json!({ "t": "art", "rid": rid }),
            };
            let _ = out.send(reply);
        });
    }

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
        let (client, out, saved_queue) = (self.client.clone(), self.out.clone(), self.saved_queue.clone());
        tokio::spawn(async move {
            let saved = match reply_rx.await {
                Ok(snapshot) => {
                    let saved = save_queue(&client, &snapshot).await;
                    if saved.is_ok() {
                        saved_queue.set(queue_key(&snapshot.songs));
                    }
                    saved
                }
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
        let saved_queue = self.saved_queue.clone();
        tokio::spawn(async move {
            match client.play_queue().await {
                Ok(saved) if !saved.songs.is_empty() => {
                    saved_queue.set(queue_key(&saved.songs));
                    let start = takeover_start(&saved.songs, saved.current.as_deref(), &state, fresh);
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

    /// Replaces the idle queue with the one saved on the server, without playing. A shared queue that
    /// was cleared is cleared here too.
    fn follow_saved_queue(&self) {
        let (client, commands, saved_queue) = (self.client.clone(), self.commands.clone(), self.saved_queue.clone());
        tokio::spawn(async move {
            let Ok(saved) = client.play_queue().await else { return };
            saved_queue.set(queue_key(&saved.songs));
            if saved.songs.is_empty() {
                let _ = commands.send(Command::Clear);
            } else {
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

/// Applies a remote's queue edit through the player. Songs to add are looked up on this device's
/// server first; ids it does not know are skipped.
async fn apply_edit(client: &Subsonic, commands: &mpsc::UnboundedSender<Command>, edit: Edit) {
    let command = match edit {
        Edit::Add { mode, ids } => {
            let songs: Vec<Song> = futures_util::stream::iter(ids)
                .map(|id| async move { client.song(&id).await })
                .buffered(LOOKUPS_AT_ONCE)
                .filter_map(|song| async move { song.ok() })
                .collect()
                .await;
            if songs.is_empty() {
                return;
            }
            match mode {
                AddMode::End => Command::Enqueue(songs),
                AddMode::Next => Command::PlayNext(songs),
                AddMode::At(at) => Command::InsertAt { at, songs },
            }
        }
        Edit::Remove(positions) => Command::Remove(positions),
        Edit::Move { from, to, track_id } => Command::Move { from, to, track_id },
        Edit::MoveBlock { indices, expect, gap } => Command::MoveBlock { indices, expect, gap },
        Edit::Clear => Command::Clear,
        Edit::ClearExceptCurrent => Command::ClearExceptCurrent,
        Edit::DropUpcoming => Command::DropUpcoming,
        Edit::Nothing => return,
    };
    let _ = commands.send(command);
}

/// The queue position a takeover starts at: the track the hub reported as playing, when the saved
/// queue holds it (the saved current track can lag behind). A click-to-play handoff (`fresh`) starts
/// at the saved current track, which the remote just picked.
fn takeover_start(songs: &[Song], current: Option<&str>, state: &Value, fresh: bool) -> usize {
    let by_current = current.and_then(|c| songs.iter().position(|s| s.id == c));
    let reported = state["trackId"].as_str().filter(|_| !fresh);
    match reported {
        Some(id) if by_current.is_some_and(|i| songs[i].id == id) => by_current.unwrap_or(0),
        Some(id) => songs.iter().position(|s| s.id == id).or(by_current).unwrap_or(0),
        None => by_current.unwrap_or(0),
    }
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

    fn queue(ids: &[&str]) -> Vec<Song> {
        ids.iter().map(|id| song(id, 300)).collect()
    }

    #[test]
    fn a_takeover_starts_on_the_track_the_hub_reported_even_if_the_saved_one_lags() {
        let songs = queue(&["a", "b", "c"]);
        let state = json!({ "trackId": "c" });
        assert_eq!(takeover_start(&songs, Some("b"), &state, false), 2);
        // The same track twice: the saved current entry, when it is that track.
        let songs = queue(&["c", "b", "c"]);
        assert_eq!(takeover_start(&songs, Some("c"), &state, false), 0);
    }

    #[test]
    fn a_click_to_play_handoff_starts_on_the_track_the_remote_picked() {
        let songs = queue(&["a", "b", "c"]);
        let state = json!({ "trackId": "c" });
        assert_eq!(takeover_start(&songs, Some("a"), &state, true), 0);
    }

    #[test]
    fn a_reported_track_not_in_the_saved_queue_falls_back_to_the_saved_current_one() {
        let songs = queue(&["a", "b"]);
        assert_eq!(
            takeover_start(&songs, Some("b"), &json!({ "trackId": "zzz" }), false),
            1
        );
        assert_eq!(takeover_start(&songs, None, &json!({}), false), 0);
    }

    #[test]
    fn a_takeover_never_starts_past_the_end() {
        let state = json!({ "trackId": "s1", "positionSec": 299.9, "playing": true });
        let pos = takeover_position(&state, Duration::from_secs(10), &song("s1", 300)).unwrap();
        assert!(pos <= Duration::from_secs_f64(299.5));
    }
}
