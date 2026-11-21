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

    #[test]
    fn a_song_plays_as_part_of_its_album_when_a_neighbour_is_the_same_album() {
        let q = Queue::new(songs(&["a", "a", "b", "c", "c"]), 0);
        assert!(q.in_album(0));
        assert!(q.in_album(1));
        assert!(!q.in_album(2), "a single from another album");
        assert!(q.in_album(4));
    }
}
