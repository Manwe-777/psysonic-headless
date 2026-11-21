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

    #[test]
    fn a_song_plays_as_part_of_its_album_when_a_neighbour_is_the_same_album() {
        let q = Queue::new(songs(&["a", "a", "b", "c", "c"]), 0);
        assert!(q.in_album(0));
        assert!(q.in_album(1));
        assert!(!q.in_album(2), "a single from another album");
        assert!(q.in_album(4));
    }
}
