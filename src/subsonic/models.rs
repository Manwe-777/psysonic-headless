use serde::Deserialize;

/// A song as the server describes it (only the fields the player uses).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Song {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub artist_id: Option<String>,
    /// Each artist on its own (OpenSubsonic), where `artist` may join several.
    #[serde(default)]
    pub artists: Vec<ArtistRef>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub album_id: Option<String>,
    #[serde(default)]
    pub cover_art: Option<String>,
    /// Seconds.
    #[serde(default)]
    pub duration: Option<u32>,
    #[serde(default)]
    pub track: Option<u32>,
    #[serde(default)]
    pub disc_number: Option<u32>,
    #[serde(default)]
    pub year: Option<u32>,
    #[serde(default)]
    pub suffix: Option<String>,
    /// Bytes of the original file.
    #[serde(default)]
    pub size: Option<u64>,
    /// ReplayGain tags (OpenSubsonic).
    #[serde(default)]
    pub replay_gain: Option<ReplayGain>,
}

/// An artist of a song (OpenSubsonic).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ArtistRef {
    #[serde(default)]
    pub name: String,
}

impl Song {
    /// The main artist: the first of `artists`, else `artist` (like Psysonic, for scrobbling).
    pub fn main_artist(&self) -> Option<&str> {
        self.artists
            .first()
            .map(|a| a.name.trim())
            .filter(|n| !n.is_empty())
            .or(self.artist.as_deref())
    }
}

/// ReplayGain values in dB and linear peaks, as OpenSubsonic servers report them.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReplayGain {
    #[serde(default)]
    pub track_gain: Option<f32>,
    #[serde(default)]
    pub album_gain: Option<f32>,
    #[serde(default)]
    pub track_peak: Option<f32>,
    #[serde(default)]
    pub album_peak: Option<f32>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AlbumWithSongs {
    #[serde(default)]
    pub song: Vec<Song>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PlaylistWithSongs {
    #[serde(default)]
    pub entry: Vec<Song>,
}

/// The queue saved on the server.
#[derive(Debug, Clone, Default)]
pub struct PlayQueue {
    pub songs: Vec<Song>,
    pub current: Option<String>,
    pub position: std::time::Duration,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct PlayQueueResponse {
    #[serde(default)]
    pub entry: Vec<Song>,
    #[serde(default)]
    pub current: Option<String>,
    /// Milliseconds.
    #[serde(default)]
    pub position: Option<u64>,
}
