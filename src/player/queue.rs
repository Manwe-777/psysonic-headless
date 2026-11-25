use crate::subsonic::Song;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

/// The songs to play, in order, and the one playing.
#[derive(Debug, Clone, Default)]
pub struct Queue {
    songs: Vec<Song>,
    index: usize,
    pub repeat: Repeat,
}

impl Queue {
    pub fn new(songs: Vec<Song>, start: usize) -> Self {
        let index = start.min(songs.len().saturating_sub(1));
        Self {
            songs,
            index,
            repeat: Repeat::Off,
        }
    }

    pub fn songs(&self) -> &[Song] {
        &self.songs
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn current(&self) -> Option<&Song> {
        self.songs.get(self.index)
    }

    pub fn get(&self, index: usize) -> Option<&Song> {
        self.songs.get(index)
    }

    /// The position that follows `index` when a track ends (repeat applies), if any.
    pub fn after(&self, index: usize) -> Option<usize> {
        if self.songs.is_empty() {
            return None;
        }
        match self.repeat {
            Repeat::One => Some(index),
            Repeat::All => Some((index + 1) % self.songs.len()),
            Repeat::Off => (index + 1 < self.songs.len()).then_some(index + 1),
        }
    }

    /// The position "next" goes to. Skipping is never held on one track, even with repeat one.
    pub fn next_index(&self) -> Option<usize> {
        if self.index + 1 < self.songs.len() {
            Some(self.index + 1)
        } else if self.repeat != Repeat::Off && !self.songs.is_empty() {
            Some(0)
        } else {
            None
        }
    }

    /// The position "previous" goes to (the first track stays on itself).
    pub fn previous_index(&self) -> usize {
        self.index.saturating_sub(1)
    }

    /// Moves to a position; false (and no change) when it is outside the queue.
    pub fn jump(&mut self, index: usize) -> bool {
        if index >= self.songs.len() {
            return false;
        }
        self.index = index;
        true
    }

    /// Where the song `id` is now, expected at `index`: there if it still is, else its first entry
    /// after the current song, else anywhere; `index` itself when it is gone.
    pub fn find_after(&self, index: usize, id: &str) -> usize {
        if self.songs.get(index).is_some_and(|s| s.id == id) {
            return index;
        }
        let at = |range: std::ops::Range<usize>| range.into_iter().find(|i| self.songs[*i].id == id);
        at(self.index + 1..self.songs.len())
            .or_else(|| at(0..self.songs.len()))
            .unwrap_or(index)
    }

    /// Adds songs at the end.
    pub fn append(&mut self, songs: Vec<Song>) {
        self.songs.extend(songs);
    }

    /// Inserts songs right after the current one ("play next").
    pub fn insert_next(&mut self, songs: Vec<Song>) {
        let at = if self.songs.is_empty() { 0 } else { self.index + 1 };
        self.songs.splice(at..at, songs);
    }

    /// Shuffles the songs after the current one; the current song and those before it stay put.
    pub fn shuffle_upcoming(&mut self, rng: &mut impl rand::Rng) {
        use rand::seq::SliceRandom;
        if self.index + 1 < self.songs.len() {
            self.songs[self.index + 1..].shuffle(rng);
        }
    }

    /// Shuffles the whole queue with the current song moved to the front, so it keeps playing.
    pub fn shuffle_all(&mut self, rng: &mut impl rand::Rng) {
        use rand::seq::SliceRandom;
        if self.songs.is_empty() {
            return;
        }
        let current = self.songs.remove(self.index);
        self.songs.shuffle(rng);
        self.songs.insert(0, current);
        self.index = 0;
    }

    /// Inserts songs before position `at` (clamped to the queue); the current song stays current.
    pub fn insert_at(&mut self, at: usize, songs: Vec<Song>) {
        let at = at.min(self.songs.len());
        let count = songs.len();
        let was_empty = self.songs.is_empty();
        self.songs.splice(at..at, songs);
        if !was_empty && at <= self.index {
            self.index += count;
        }
    }

    /// The positions a remote meant, given as (position, song id) as it saw them: each position when
    /// it still holds that song, otherwise the first other entry with it. Songs that are gone are left out.
    pub fn pick(&self, positions: &[(usize, String)]) -> Vec<usize> {
        let mut used = std::collections::HashSet::new();
        let mut picked = Vec::new();
        for (index, id) in positions {
            let here = self.songs.get(*index).is_some_and(|s| s.id == *id) && !used.contains(index);
            let at = if here {
                Some(*index)
            } else {
                (0..self.songs.len()).find(|i| self.songs[*i].id == *id && !used.contains(i))
            };
            if let Some(at) = at {
                used.insert(at);
                picked.push(at);
            }
        }
        picked
    }

    /// Removes the songs at `positions`, except the current one when `keep_current` (it is playing).
    /// The current song stays current; when it was removed, the next one left takes its place.
    /// False when nothing was removed.
    pub fn remove(&mut self, positions: &[usize], keep_current: bool) -> bool {
        let mut doomed: std::collections::HashSet<usize> =
            positions.iter().copied().filter(|i| *i < self.songs.len()).collect();
        if keep_current {
            doomed.remove(&self.index);
        }
        if doomed.is_empty() {
            return false;
        }
        let kept_before = (0..self.index).filter(|i| !doomed.contains(i)).count();
        let mut i = 0;
        self.songs.retain(|_| {
            i += 1;
            !doomed.contains(&(i - 1))
        });
        self.index = kept_before.min(self.songs.len().saturating_sub(1));
        true
    }

    /// Moves the song at `from` so it ends up at `to` (taken out, then put back); the current song
    /// stays current. False when either is outside the queue or nothing would move.
    pub fn move_one(&mut self, from: usize, to: usize) -> bool {
        if from >= self.songs.len() || to >= self.songs.len() || from == to {
            return false;
        }
        let song = self.songs.remove(from);
        self.songs.insert(to, song);
        self.index = if self.index == from {
            to
        } else if from < self.index && to >= self.index {
            self.index - 1
        } else if from > self.index && to <= self.index {
            self.index + 1
        } else {
            self.index
        };
        true
    }

    /// Moves the songs at `indices` as one block, in queue order, into the gap before `gap` (counted in
    /// the queue as it is now). The current song stays current. False when nothing would move.
    pub fn move_block(&mut self, indices: &[usize], gap: usize) -> bool {
        let mut moving: Vec<usize> = indices.iter().copied().filter(|i| *i < self.songs.len()).collect();
        moving.sort_unstable();
        moving.dedup();
        if moving.is_empty() {
            return false;
        }
        let gap = gap.min(self.songs.len());
        let at = gap - moving.iter().filter(|i| **i < gap).count();
        let mut order: Vec<usize> = (0..self.songs.len())
            .filter(|i| moving.binary_search(i).is_err())
            .collect();
        order.splice(at..at, moving.iter().copied());
        if order.iter().enumerate().all(|(n, i)| n == *i) {
            return false;
        }
        let current = self.index;
        let mut old: Vec<Option<Song>> = std::mem::take(&mut self.songs).into_iter().map(Some).collect();
        self.songs = order
            .iter()
            .map(|i| old[*i].take().expect("each position appears once"))
            .collect();
        self.index = order.iter().position(|i| *i == current).unwrap_or(0);
        true
    }

    /// Empties the queue.
    pub fn clear(&mut self) {
        self.songs.clear();
        self.index = 0;
    }

    /// Keeps only the current song.
    pub fn keep_only_current(&mut self) {
        if let Some(song) = self.songs.get(self.index).cloned() {
            self.songs = vec![song];
        } else {
            self.songs.clear();
        }
        self.index = 0;
    }

    /// Drops the songs after the current one.
    pub fn drop_upcoming(&mut self) {
        self.songs.truncate(self.index + 1);
    }

    /// Whether the song at `index` plays as part of its album in order: a neighbour in the queue is
    /// the same album (what ReplayGain's auto mode uses to choose album gain).
    pub fn in_album(&self, index: usize) -> bool {
        let Some(album) = self.songs.get(index).and_then(|s| s.album_id.as_deref()) else {
            return false;
        };
        let same =
            |i: Option<usize>| i.and_then(|i| self.songs.get(i)).and_then(|s| s.album_id.as_deref()) == Some(album);
        same(index.checked_sub(1)) || same(Some(index + 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn songs(albums: &[&str]) -> Vec<Song> {
        albums
            .iter()
            .enumerate()
            .map(|(i, a)| serde_json::from_value(serde_json::json!({ "id": format!("s{i}"), "albumId": a })).unwrap())
            .collect()
    }

    #[test]
    fn the_end_of_the_queue_stops_unless_repeating() {
        let mut q = Queue::new(songs(&["a", "a", "a"]), 2);
        assert_eq!(q.after(2), None);
        q.repeat = Repeat::All;
        assert_eq!(q.after(2), Some(0));
        q.repeat = Repeat::One;
        assert_eq!(q.after(2), Some(2));
    }

    #[test]
    fn next_moves_on_even_when_repeating_one_track() {
        let mut q = Queue::new(songs(&["a", "a"]), 0);
        q.repeat = Repeat::One;
        assert_eq!(q.next_index(), Some(1));
        q.jump(1);
        assert_eq!(q.next_index(), Some(0), "repeat wraps next around");
        q.repeat = Repeat::Off;
        assert_eq!(q.next_index(), None);
    }

    #[test]
    fn previous_stays_on_the_first_track() {
        let q = Queue::new(songs(&["a", "a"]), 0);
        assert_eq!(q.previous_index(), 0);
    }

    #[test]
    fn jumping_outside_the_queue_changes_nothing() {
        let mut q = Queue::new(songs(&["a", "a"]), 1);
        assert!(!q.jump(5));
        assert_eq!(q.index(), 1);
    }

    #[test]
    fn a_start_past_the_end_starts_at_the_last_track() {
        assert_eq!(Queue::new(songs(&["a", "b"]), 9).index(), 1);
    }

    fn ids(q: &Queue) -> Vec<String> {
        q.songs().iter().map(|s| s.id.clone()).collect()
    }

    #[test]
    fn play_next_goes_right_after_the_current_song() {
        let mut q = Queue::new(songs(&["a", "a", "a"]), 1);
        q.insert_next(
            songs(&["x"])
                .into_iter()
                .map(|s| Song { id: "new".into(), ..s })
                .collect(),
        );
        assert_eq!(ids(&q), ["s0", "s1", "new", "s2"]);
        assert_eq!(q.index(), 1, "the current song does not change");
    }

    #[test]
    fn adding_to_an_empty_queue_with_play_next_starts_at_the_top() {
        let mut q = Queue::default();
        q.insert_next(songs(&["a"]));
        assert_eq!(ids(&q), ["s0"]);
    }

    #[test]
    fn shuffling_the_upcoming_songs_leaves_the_played_and_current_ones_alone() {
        let mut q = Queue::new(songs(&["a"; 20]), 5);
        let before = ids(&q);
        q.shuffle_upcoming(&mut rand::rng());
        assert_eq!(ids(&q)[..6], before[..6]);
        let mut rest = ids(&q)[6..].to_vec();
        rest.sort();
        let mut expected = before[6..].to_vec();
        expected.sort();
        assert_eq!(rest, expected, "the same songs, possibly in another order");
    }

    #[test]
    fn shuffling_everything_keeps_the_current_song_playing_first() {
        let mut q = Queue::new(songs(&["a"; 10]), 4);
        q.shuffle_all(&mut rand::rng());
        assert_eq!(q.index(), 0);
        assert_eq!(q.current().unwrap().id, "s4");
        assert_eq!(q.songs().len(), 10);
    }

    fn named(names: &[&str]) -> Vec<Song> {
        names
            .iter()
            .map(|n| serde_json::from_value(serde_json::json!({ "id": n })).unwrap())
            .collect()
    }

    fn current(q: &Queue) -> &str {
        &q.current().unwrap().id
    }

    #[test]
    fn inserting_before_the_current_song_keeps_it_playing() {
        let mut q = Queue::new(named(&["a", "b", "c"]), 1);
        q.insert_at(0, named(&["x", "y"]));
        assert_eq!(ids(&q), ["x", "y", "a", "b", "c"]);
        assert_eq!(current(&q), "b");
        q.insert_at(99, named(&["z"]));
        assert_eq!(ids(&q).last().unwrap(), "z", "a position past the end adds at the end");
        assert_eq!(current(&q), "b");
    }

    #[test]
    fn a_remote_s_positions_find_their_songs_even_after_the_queue_moved() {
        let q = Queue::new(named(&["a", "b", "c", "b"]), 0);
        // "c" moved from 1 to 2 since the remote looked; "gone" is no longer there.
        let picked = q.pick(&[(1, "c".into()), (3, "b".into()), (0, "gone".into())]);
        assert_eq!(picked, [2, 3]);
        // The same song twice takes two different entries.
        assert_eq!(q.pick(&[(1, "b".into()), (1, "b".into())]), [1, 3]);
    }

    #[test]
    fn removing_songs_never_removes_the_one_playing() {
        let mut q = Queue::new(named(&["a", "b", "c", "d"]), 2);
        assert!(q.remove(&[0, 2, 3], true));
        assert_eq!(ids(&q), ["b", "c"]);
        assert_eq!(current(&q), "c");
        assert!(
            !q.remove(&[1], true),
            "only the playing song asked for: nothing changes"
        );
    }

    #[test]
    fn removing_the_current_song_while_stopped_moves_on_to_the_next_one_left() {
        let mut q = Queue::new(named(&["a", "b", "c"]), 1);
        assert!(q.remove(&[1], false));
        assert_eq!(current(&q), "c");
        let mut q = Queue::new(named(&["a", "b"]), 1);
        q.remove(&[1], false);
        assert_eq!(current(&q), "a", "the last one gone: the one before");
    }

    #[test]
    fn moving_one_song_keeps_the_current_song_playing() {
        let mut q = Queue::new(named(&["a", "b", "c", "d"]), 1);
        assert!(q.move_one(3, 0));
        assert_eq!(ids(&q), ["d", "a", "b", "c"]);
        assert_eq!(current(&q), "b");
        assert!(q.move_one(2, 3), "the current song itself");
        assert_eq!(current(&q), "b");
        assert_eq!(q.index(), 3);
        assert!(!q.move_one(0, 9), "a target outside the queue");
        assert!(!q.move_one(1, 1));
    }

    #[test]
    fn moving_a_block_puts_it_in_the_gap_the_remote_showed() {
        let mut q = Queue::new(named(&["a", "b", "c", "d", "e"]), 2);
        // Move "a" and "d" into the gap before "e" (gap 4, counted before the move).
        assert!(q.move_block(&[3, 0], 4));
        assert_eq!(ids(&q), ["b", "c", "a", "d", "e"]);
        assert_eq!(current(&q), "c");
        assert!(!q.move_block(&[2, 3], 2), "already there: nothing moves");
    }

    #[test]
    fn clearing_keeping_the_current_song_or_dropping_what_is_next() {
        let mut q = Queue::new(named(&["a", "b", "c"]), 1);
        q.drop_upcoming();
        assert_eq!(ids(&q), ["a", "b"]);
        q.keep_only_current();
        assert_eq!(ids(&q), ["b"]);
        assert_eq!(current(&q), "b");
        q.clear();
        assert!(q.songs().is_empty());
    }

    #[test]
    fn a_song_plays_as_part_of_its_album_when_a_neighbour_is_the_same_album() {
        let q = Queue::new(songs(&["a", "a", "b", "c", "c"]), 0);
        assert!(q.in_album(0));
        assert!(q.in_album(1));
        assert!(!q.in_album(2), "a single from another album");
        assert!(q.in_album(4));
    }
}
