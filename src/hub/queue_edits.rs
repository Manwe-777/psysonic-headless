//! Queue edits remotes send to the playing device (after the desktop's `hubQueueEdits.ts`). Only the
//! playing device saves the shared queue, so a remote describes its edit (which songs, which
//! positions) as a hub command and the player applies it. The hub drops command arguments over
//! 2048 bytes of JSON, so long lists of songs to add arrive in several parts.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;

/// Parts of an add that never complete are dropped after this long.
const ADD_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_PARTS: u64 = 200;
/// More positions than this in one move is not a real selection.
const MAX_RUN_POSITIONS: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddMode {
    /// At the end of the queue.
    End,
    /// Right after the song playing.
    Next,
    /// Before a queue position.
    At(usize),
}

/// A queue edit from a remote.
#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    /// Songs (by id, looked up on this device's server) to add.
    Add {
        mode: AddMode,
        ids: Vec<String>,
    },
    /// The entries at these positions, with the song each held when the remote saw it.
    Remove(Vec<(usize, String)>),
    /// The song at `from` ends up at `to`.
    Move {
        from: usize,
        to: usize,
        track_id: String,
    },
    /// These positions move as one block into the gap before `gap`; `expect` holds the songs they
    /// must still hold (none for a large selection sent as runs of positions).
    MoveBlock {
        indices: Vec<usize>,
        expect: Option<Vec<String>>,
        gap: usize,
    },
    Clear,
    ClearExceptCurrent,
    DropUpcoming,
    /// An edit that changes nothing here (malformed, or undo/redo, which this player keeps no
    /// history for). The remote still hears that the queue is as saved, so it drops what it showed.
    Nothing,
}

struct PendingAdd {
    mode: AddMode,
    parts: Vec<Option<Vec<String>>>,
    since: Instant,
}

/// Reads queue edit commands; keeps the parts of adds until each is complete.
#[derive(Default)]
pub struct EditReader {
    pending: HashMap<String, PendingAdd>,
}

impl EditReader {
    /// The edit a command asks for; None when `action` is not a queue edit, or for an add that is
    /// still waiting for its other parts.
    pub fn read(&mut self, action: &str, args: &Value, now: Instant) -> Option<Edit> {
        Some(match action {
            "queueAdd" => return self.add_part(args, now),
            "removeQueue" => read_positions(args).map_or(Edit::Nothing, Edit::Remove),
            "moveQueue" => read_move(args).unwrap_or(Edit::Nothing),
            "moveQueueItems" => read_move_block(args).unwrap_or(Edit::Nothing),
            "clearQueue" => Edit::Clear,
            "clearQueueExceptCurrent" => Edit::ClearExceptCurrent,
            "pruneUpcoming" => Edit::DropUpcoming,
            "undoQueue" | "redoQueue" => Edit::Nothing,
            _ => return None,
        })
    }

    fn add_part(&mut self, args: &Value, now: Instant) -> Option<Edit> {
        self.pending.retain(|_, p| now.duration_since(p.since) < ADD_TIMEOUT);
        let batch = args["batch"].as_str().filter(|b| !b.is_empty() && b.len() <= 40)?;
        let parts = args["parts"].as_u64().filter(|n| (1..=MAX_PARTS).contains(n))? as usize;
        let part = args["part"].as_u64().filter(|n| (*n as usize) < parts)? as usize;
        let mode = match args["mode"].as_str()? {
            "end" => AddMode::End,
            "next" => AddMode::Next,
            "at" => AddMode::At(args["at"].as_u64().unwrap_or(0) as usize),
            _ => return None,
        };
        let ids = string_list(&args["ids"])?;
        let entry = self.pending.entry(batch.to_string()).or_insert_with(|| PendingAdd {
            mode,
            parts: vec![None; parts],
            since: now,
        });
        if entry.parts.len() != parts {
            return None;
        }
        entry.parts[part] = Some(ids);
        if entry.parts.iter().any(Option::is_none) {
            return None;
        }
        let done = self.pending.remove(batch)?;
        Some(Edit::Add {
            mode: done.mode,
            ids: done.parts.into_iter().flatten().flatten().collect(),
        })
    }
}

fn index(v: &Value) -> Option<usize> {
    v.as_u64().map(|n| n as usize)
}

fn string_list(v: &Value) -> Option<Vec<String>> {
    v.as_array()?.iter().map(|s| s.as_str().map(str::to_string)).collect()
}

/// `{ i: [positions], id: [song ids] }`, the same length; None when malformed.
fn read_positions(args: &Value) -> Option<Vec<(usize, String)>> {
    let i: Option<Vec<usize>> = args["i"].as_array()?.iter().map(index).collect();
    let (i, id) = (i?, string_list(&args["id"])?);
    (i.len() == id.len()).then(|| i.into_iter().zip(id).collect())
}

fn read_move(args: &Value) -> Option<Edit> {
    Some(Edit::Move {
        from: index(&args["from"])?,
        to: index(&args["to"])?,
        track_id: args["trackId"].as_str()?.to_string(),
    })
}

fn read_move_block(args: &Value) -> Option<Edit> {
    let gap = index(&args["gap"])?;
    let (indices, expect) = match read_positions(args) {
        Some(positions) => {
            let (i, id) = positions.into_iter().unzip();
            (i, Some(id))
        }
        // Large selections travel without song ids, as runs of positions.
        None => (from_runs(&args["r"])?, None),
    };
    (!indices.is_empty()).then_some(Edit::MoveBlock { indices, expect, gap })
}

/// Positions from runs `[[start, count], ...]`; None when malformed or implausibly large.
fn from_runs(raw: &Value) -> Option<Vec<usize>> {
    let mut out = Vec::new();
    for run in raw.as_array()? {
        let run = run.as_array().filter(|r| r.len() == 2)?;
        let (start, count) = (index(&run[0])?, index(&run[1]).filter(|c| *c >= 1)?);
        if out.len() + count > MAX_RUN_POSITIONS {
            return None;
        }
        out.extend(start..start + count);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read(action: &str, args: Value) -> Option<Edit> {
        EditReader::default().read(action, &args, Instant::now())
    }

    #[test]
    fn an_add_in_several_parts_waits_for_all_of_them_in_any_order() {
        let mut reader = EditReader::default();
        let now = Instant::now();
        let part =
            |k: u64, ids: Value| json!({ "batch": "b1", "part": k, "parts": 2, "mode": "at", "at": 3, "ids": ids });
        assert_eq!(reader.read("queueAdd", &part(1, json!(["c"])), now), None);
        assert_eq!(
            reader.read("queueAdd", &part(0, json!(["a", "b"])), now),
            Some(Edit::Add {
                mode: AddMode::At(3),
                ids: vec!["a".into(), "b".into(), "c".into()],
            })
        );
    }

    #[test]
    fn an_add_whose_parts_stop_coming_is_dropped() {
        let mut reader = EditReader::default();
        let start = Instant::now();
        let first = json!({ "batch": "b1", "part": 0, "parts": 2, "mode": "end", "ids": ["a"] });
        let second = json!({ "batch": "b1", "part": 1, "parts": 2, "mode": "end", "ids": ["b"] });
        reader.read("queueAdd", &first, start);
        let later = start + Duration::from_secs(16);
        assert_eq!(reader.read("queueAdd", &second, later), None, "the first part is gone");
    }

    #[test]
    fn a_malformed_add_part_is_ignored() {
        assert_eq!(
            read(
                "queueAdd",
                json!({ "batch": "b", "part": 2, "parts": 2, "mode": "end", "ids": [] })
            ),
            None
        );
        assert_eq!(
            read(
                "queueAdd",
                json!({ "batch": "b", "part": 0, "parts": 1, "mode": "sideways", "ids": [] })
            ),
            None
        );
        assert_eq!(
            read(
                "queueAdd",
                json!({ "batch": "b", "part": 0, "parts": 1, "mode": "end", "ids": [1] })
            ),
            None
        );
    }

    #[test]
    fn removals_and_moves_carry_the_songs_the_remote_saw() {
        assert_eq!(
            read("removeQueue", json!({ "i": [4, 1], "id": ["x", "y"] })),
            Some(Edit::Remove(vec![(4, "x".into()), (1, "y".into())]))
        );
        assert_eq!(
            read("moveQueue", json!({ "from": 2, "to": 0, "trackId": "x" })),
            Some(Edit::Move {
                from: 2,
                to: 0,
                track_id: "x".into()
            })
        );
        assert_eq!(
            read("moveQueueItems", json!({ "i": [1, 2], "id": ["a", "b"], "gap": 5 })),
            Some(Edit::MoveBlock {
                indices: vec![1, 2],
                expect: Some(vec!["a".into(), "b".into()]),
                gap: 5
            })
        );
    }

    #[test]
    fn a_large_move_comes_as_runs_of_positions() {
        assert_eq!(
            read("moveQueueItems", json!({ "r": [[0, 2], [10, 3]], "gap": 20 })),
            Some(Edit::MoveBlock {
                indices: vec![0, 1, 10, 11, 12],
                expect: None,
                gap: 20
            })
        );
        assert_eq!(
            read("moveQueueItems", json!({ "r": [[0, 0]], "gap": 1 })),
            Some(Edit::Nothing)
        );
        assert_eq!(
            read("moveQueueItems", json!({ "r": [[0, 200_000]], "gap": 1 })),
            Some(Edit::Nothing)
        );
    }

    #[test]
    fn a_malformed_edit_or_undo_changes_nothing_but_is_still_answered() {
        assert_eq!(
            read("removeQueue", json!({ "i": [1, 2], "id": ["a"] })),
            Some(Edit::Nothing)
        );
        assert_eq!(
            read("moveQueue", json!({ "from": -1, "to": 0, "trackId": "x" })),
            Some(Edit::Nothing)
        );
        assert_eq!(read("undoQueue", json!(null)), Some(Edit::Nothing));
    }

    #[test]
    fn other_commands_are_not_queue_edits() {
        assert_eq!(read("play", json!(null)), None);
        assert_eq!(read("playIndex", json!({ "index": 1 })), None);
    }
}
