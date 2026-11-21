use crate::config::ReplayGainMode;
use crate::subsonic::Song;

/// The linear gain for a song under a ReplayGain mode. `in_album` says whether the song plays as part
/// of its album in order (what `auto` uses to pick album gain). Missing tags fall back to the other
/// kind, then to no change. The gain never pushes the song's peak above full scale.
pub fn replay_gain_factor(song: &Song, mode: ReplayGainMode, in_album: bool) -> f32 {
    let Some(rg) = song.replay_gain.as_ref() else {
        return 1.0;
    };
    let album = match mode {
        ReplayGainMode::Off => return 1.0,
        ReplayGainMode::Track => false,
        ReplayGainMode::Album => true,
        ReplayGainMode::Auto => in_album,
    };
    let (gain, peak) = if album {
        (rg.album_gain.or(rg.track_gain), rg.album_peak.or(rg.track_peak))
    } else {
        (rg.track_gain.or(rg.album_gain), rg.track_peak.or(rg.album_peak))
    };
    let Some(gain_db) = gain else { return 1.0 };
    let factor = 10f32.powf(gain_db / 20.0);
    match peak {
        Some(p) if p > 0.0 => factor.min(1.0 / p),
        _ => factor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subsonic::ReplayGain;

    fn song(rg: ReplayGain) -> Song {
        serde_json::from_value::<Song>(serde_json::json!({ "id": "s" }))
            .map(|s| Song {
                replay_gain: Some(rg),
                ..s
            })
            .unwrap()
    }

    #[test]
    fn off_or_untagged_songs_play_unchanged() {
        let s = song(ReplayGain {
            track_gain: Some(-6.0),
            ..Default::default()
        });
        assert_eq!(replay_gain_factor(&s, ReplayGainMode::Off, false), 1.0);
        let untagged = Song { replay_gain: None, ..s };
        assert_eq!(replay_gain_factor(&untagged, ReplayGainMode::Track, false), 1.0);
    }

    #[test]
    fn minus_six_db_is_about_half_amplitude() {
        let s = song(ReplayGain {
            track_gain: Some(-6.0206),
            ..Default::default()
        });
        assert!((replay_gain_factor(&s, ReplayGainMode::Track, false) - 0.5).abs() < 0.001);
    }

    #[test]
    fn auto_uses_album_gain_only_while_the_album_plays_in_order() {
        let s = song(ReplayGain {
            track_gain: Some(-3.0),
            album_gain: Some(-9.0),
            ..Default::default()
        });
        let album = replay_gain_factor(&s, ReplayGainMode::Album, false);
        assert_eq!(replay_gain_factor(&s, ReplayGainMode::Auto, true), album);
        assert_eq!(
            replay_gain_factor(&s, ReplayGainMode::Auto, false),
            replay_gain_factor(&s, ReplayGainMode::Track, false)
        );
    }

    #[test]
    fn a_missing_kind_falls_back_to_the_other() {
        let only_track = song(ReplayGain {
            track_gain: Some(-6.0206),
            ..Default::default()
        });
        assert!((replay_gain_factor(&only_track, ReplayGainMode::Album, true) - 0.5).abs() < 0.001);
    }

    #[test]
    fn a_boost_never_clips_the_peak() {
        let s = song(ReplayGain {
            track_gain: Some(6.0),
            track_peak: Some(0.8),
            ..Default::default()
        });
        assert!((replay_gain_factor(&s, ReplayGainMode::Track, false) - 1.25).abs() < 0.001);
    }
}
