//! Command-line flags; every option can also come from a `PSYSONIC_*` environment variable, so the
//! same binary runs by hand or as a service with an `EnvironmentFile`. Secrets are read from an
//! environment variable or a file, never from a flag value (command lines are visible to all users).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "psysonic-headless",
    version = VERSION,
    about = "Headless Psysonic player for a Navidrome server"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Join the hub as a player and play what it says (the service mode).
    Run(RunArgs),
    /// List the audio output devices.
    Devices,
    /// Play an album, a playlist or songs from the server, with keyboard controls.
    Play(PlayArgs),
    /// Install (or reconfigure) the player as a systemd service; run again to change settings.
    Install(InstallArgs),
    /// Update to the latest release (keeps the settings).
    Update(UpdateArgs),
    /// Remove the service and the binary; the settings stay unless --purge.
    Uninstall(UninstallArgs),
    /// Sign in to Last.fm (allow access in a browser) and save the session key to a file.
    LastfmLogin(LastfmLoginArgs),
}

#[derive(Debug, Args)]
pub struct LastfmLoginArgs {
    /// Where to save the session key (then pass it to `run` as --lastfm-session-file).
    #[arg(long)]
    pub session_file: PathBuf,
}

/// The release version (set by the release build), or the crate's with `-dev`.
pub const VERSION: &str = match option_env!("PSYSONIC_HEADLESS_VERSION") {
    Some(v) => v,
    None => concat!(env!("CARGO_PKG_VERSION"), "-dev"),
};

/// Settings left out keep their current value; on a first install the missing required ones are
/// asked for. The server password and hub token are asked for without echo (or read from a file).
#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Navidrome / Subsonic server address.
    #[arg(long)]
    pub server_url: Option<String>,
    /// Server user name.
    #[arg(long)]
    pub server_user: Option<String>,
    /// Read the server password from this file instead of asking.
    #[arg(long)]
    pub server_password_file: Option<PathBuf>,
    /// Psysonic Hub address.
    #[arg(long)]
    pub hub_url: Option<String>,
    /// Read the hub's device token from this file instead of asking.
    #[arg(long)]
    pub hub_token_file: Option<PathBuf>,
    /// Ask for the server password and hub token again.
    #[arg(long)]
    pub new_secrets: bool,
    /// How this player appears on the hub (default: the host name).
    #[arg(long)]
    pub device_name: Option<String>,
    /// Output device (see `psysonic-headless devices`); empty = the system default.
    #[arg(long)]
    pub audio_device: Option<String>,
    /// Maximum stream bitrate in kbps (0 = original).
    #[arg(long, value_parser = parse_bitrate)]
    pub max_bitrate: Option<u32>,
    #[arg(long, value_enum)]
    pub format: Option<StreamFormat>,
    #[arg(long, value_enum)]
    pub replay_gain: Option<ReplayGainMode>,
    #[arg(long, value_enum)]
    pub cache: Option<CacheMode>,
    #[arg(long)]
    pub cache_max_mb: Option<u64>,
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
    pub volume: Option<u8>,
    #[arg(long, value_enum)]
    pub scrobble: Option<OnOff>,
    #[arg(long, value_parser = clap::value_parser!(u8).range(crate::scrobble::PERCENT_RANGE))]
    pub scrobble_percent: Option<u8>,
    /// Scrobble to Last.fm: shows a link to allow access (also to sign in again).
    #[arg(long, conflicts_with = "no_lastfm")]
    pub lastfm: bool,
    /// Stop scrobbling to Last.fm and forget the session.
    #[arg(long)]
    pub no_lastfm: bool,
    /// Install without starting the service.
    #[arg(long)]
    pub no_start: bool,
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// Only say whether a newer release exists.
    #[arg(long)]
    pub check: bool,
    /// Reinstall even when this is the latest.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct UninstallArgs {
    /// Also delete the settings, the secrets and the track cache.
    #[arg(long)]
    pub purge: bool,
}

#[derive(Debug, Args)]
pub struct PlayArgs {
    #[command(flatten)]
    pub server: ServerArgs,
    #[command(flatten)]
    pub playback: PlaybackArgs,
    #[command(flatten)]
    pub scrobble: ScrobbleArgs,
    /// Album id to play.
    #[arg(long, conflicts_with_all = ["playlist", "song"])]
    pub album: Option<String>,
    /// Playlist id to play.
    #[arg(long, conflicts_with = "song")]
    pub playlist: Option<String>,
    /// Song ids to play, in order.
    #[arg(long, num_args = 1..)]
    pub song: Vec<String>,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub server: ServerArgs,
    #[command(flatten)]
    pub hub: HubArgs,
    #[command(flatten)]
    pub playback: PlaybackArgs,
    #[command(flatten)]
    pub scrobble: ScrobbleArgs,
}

#[derive(Debug, Args, Clone)]
pub struct ScrobbleArgs {
    /// Report plays to the server (play counts, recently played) and to Last.fm when signed in.
    #[arg(long, env = "PSYSONIC_SCROBBLE", value_enum, default_value_t = OnOff::On)]
    pub scrobble: OnOff,
    /// How much of a track has to play before it counts, in percent (25 to 90, like Psysonic).
    #[arg(long, env = "PSYSONIC_SCROBBLE_PERCENT", default_value_t = 50,
        value_parser = clap::value_parser!(u8).range(crate::scrobble::PERCENT_RANGE))]
    pub scrobble_percent: u8,
    /// File holding the Last.fm session key (from `lastfm-login`); without it, no Last.fm.
    #[arg(long, env = "PSYSONIC_LASTFM_SESSION_FILE")]
    pub lastfm_session_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

#[derive(Debug, Args)]
pub struct HubArgs {
    /// Psysonic Hub address, e.g. https://hub.example.com or http://192.168.1.2:5533
    #[arg(long, env = "PSYSONIC_HUB_URL")]
    pub hub_url: String,
    /// File holding the hub's device token (or set PSYSONIC_HUB_TOKEN).
    #[arg(long, env = "PSYSONIC_HUB_TOKEN_FILE")]
    pub hub_token_file: Option<PathBuf>,
    /// How this player appears on the hub; default: the host name.
    #[arg(long, env = "PSYSONIC_DEVICE_NAME")]
    pub device_name: Option<String>,
    /// A stable id on the hub (4 to 64 letters, digits, _ or -); default: derived from host and name.
    #[arg(long, env = "PSYSONIC_DEVICE_ID", value_parser = parse_device_id)]
    pub device_id: Option<String>,
}

impl HubArgs {
    /// The token from `PSYSONIC_HUB_TOKEN` or the token file.
    pub fn token(&self) -> Result<String> {
        secret("PSYSONIC_HUB_TOKEN", self.hub_token_file.as_ref(), "--hub-token-file")
    }
}

fn parse_device_id(s: &str) -> Result<String, String> {
    let ok = (4..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(s.to_string())
    } else {
        Err("use 4 to 64 letters, digits, _ or -".into())
    }
}

#[derive(Debug, Args)]
pub struct ServerArgs {
    /// Navidrome / Subsonic server address, e.g. https://music.example.com
    #[arg(long, env = "PSYSONIC_SERVER_URL")]
    pub server_url: String,
    /// Server user name.
    #[arg(long, env = "PSYSONIC_SERVER_USER")]
    pub server_user: String,
    /// File holding the server password (or set PSYSONIC_SERVER_PASSWORD).
    #[arg(long, env = "PSYSONIC_SERVER_PASSWORD_FILE")]
    pub server_password_file: Option<PathBuf>,
}

impl ServerArgs {
    /// The password from `PSYSONIC_SERVER_PASSWORD` or the password file.
    pub fn password(&self) -> Result<String> {
        secret(
            "PSYSONIC_SERVER_PASSWORD",
            self.server_password_file.as_ref(),
            "--server-password-file",
        )
    }
}

#[derive(Debug, Args, Clone)]
pub struct PlaybackArgs {
    /// Output device name (see `psysonic-headless devices`); default: the system default.
    #[arg(long, env = "PSYSONIC_AUDIO_DEVICE")]
    pub audio_device: Option<String>,
    /// Maximum stream bitrate in kbps: 0 = original (no transcoding), or 320, 256, 192, 128, 96, 64.
    #[arg(long, env = "PSYSONIC_MAX_BITRATE", default_value_t = 0, value_parser = parse_bitrate)]
    pub max_bitrate: u32,
    /// Transcode format when a bitrate cap applies: auto lets the server choose.
    #[arg(long, env = "PSYSONIC_FORMAT", value_enum, default_value_t = StreamFormat::Auto)]
    pub format: StreamFormat,
    /// ReplayGain: off, track, album, or auto (album gain while an album plays in order).
    #[arg(long, env = "PSYSONIC_REPLAY_GAIN", value_enum, default_value_t = ReplayGainMode::Off)]
    pub replay_gain: ReplayGainMode,
    /// Track cache: `original` keeps the original files of the tracks coming up (they play whatever
    /// the bitrate cap; like the desktop app), `streamed` keeps tracks as streamed with the settings
    /// above, `off` keeps nothing.
    #[arg(long, env = "PSYSONIC_CACHE", value_enum, default_value_t = CacheMode::Original)]
    pub cache: CacheMode,
    /// Track cache size in MB (0 = off).
    #[arg(long, env = "PSYSONIC_CACHE_MAX_MB", default_value_t = 256)]
    pub cache_max_mb: u64,
    /// Where the track cache lives; default: systemd's CACHE_DIRECTORY, $XDG_CACHE_HOME or ~/.cache.
    #[arg(long, env = "PSYSONIC_CACHE_DIR")]
    pub cache_dir: Option<PathBuf>,
    /// Starting volume, 0 to 100.
    #[arg(long, env = "PSYSONIC_VOLUME", default_value_t = 100, value_parser = clap::value_parser!(u8).range(0..=100))]
    pub volume: u8,
}

/// The stream bitrate caps Psysonic offers (0 = original).
pub const BITRATE_OPTIONS: [u32; 7] = [0, 320, 256, 192, 128, 96, 64];

fn parse_bitrate(s: &str) -> Result<u32, String> {
    let n: u32 = s.parse().map_err(|_| format!("not a number: {s}"))?;
    if BITRATE_OPTIONS.contains(&n) {
        Ok(n)
    } else {
        Err(format!("use one of {BITRATE_OPTIONS:?}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum StreamFormat {
    Auto,
    Mp3,
    Opus,
    Aac,
}

impl StreamFormat {
    /// The `format` parameter for the server, or none to let it choose.
    pub fn param(self) -> Option<&'static str> {
        match self {
            StreamFormat::Auto => None,
            StreamFormat::Mp3 => Some("mp3"),
            StreamFormat::Opus => Some("opus"),
            StreamFormat::Aac => Some("aac"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CacheMode {
    Original,
    Streamed,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ReplayGainMode {
    Off,
    Track,
    Album,
    Auto,
}

/// A secret from an environment variable, or else from a file (a trailing line ending is dropped;
/// other whitespace is kept, since a password may end in a space).
fn secret(env_name: &str, file: Option<&PathBuf>, flag: &str) -> Result<String> {
    if let Ok(value) = std::env::var(env_name)
        && !value.is_empty()
    {
        return Ok(value);
    }
    let Some(path) = file else {
        bail!("set {env_name} or {flag}");
    };
    read_secret_file(path)
}

/// A secret from a file: a trailing line ending is dropped, other whitespace kept.
pub fn read_secret_file(path: &std::path::Path) -> Result<String> {
    let value = std::fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    let value = value.strip_suffix('\n').unwrap_or(&value);
    let value = value.strip_suffix('\r').unwrap_or(value).to_string();
    if value.is_empty() {
        bail!("{} is empty", path.display());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_bitrates_psysonic_offers_are_accepted() {
        assert_eq!(parse_bitrate("0"), Ok(0));
        assert_eq!(parse_bitrate("320"), Ok(320));
        assert!(parse_bitrate("300").is_err());
        assert!(parse_bitrate("lots").is_err());
    }

    #[test]
    fn device_ids_follow_the_hub_rules() {
        assert!(parse_device_id("living-room_pi").is_ok());
        assert!(parse_device_id("abc").is_err());
        assert!(parse_device_id("has space").is_err());
        assert!(parse_device_id(&"x".repeat(65)).is_err());
    }

    #[test]
    fn auto_format_lets_the_server_choose() {
        assert_eq!(StreamFormat::Auto.param(), None);
        assert_eq!(StreamFormat::Opus.param(), Some("opus"));
    }

    #[test]
    fn a_secret_file_is_read_and_trimmed() {
        let dir = std::env::temp_dir().join(format!("psh-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("pw");
        std::fs::write(&file, "hunter22\n").unwrap();
        assert_eq!(secret("PSH_TEST_UNSET_VAR", Some(&file), "--x").unwrap(), "hunter22");
        std::fs::write(&file, "ends in a space \r\n").unwrap();
        assert_eq!(
            secret("PSH_TEST_UNSET_VAR", Some(&file), "--x").unwrap(),
            "ends in a space "
        );
        std::fs::write(&file, "\n").unwrap();
        assert!(secret("PSH_TEST_UNSET_VAR", Some(&file), "--x").is_err());
        assert!(secret("PSH_TEST_UNSET_VAR", None, "--x").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
