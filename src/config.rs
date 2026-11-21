//! Command-line flags; every option can also come from a `PSYSONIC_*` environment variable, so the
//! same binary runs by hand or as a service with an `EnvironmentFile`. Secrets are read from an
//! environment variable or a file, never from a flag value (command lines are visible to all users).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "psysonic-headless",
    version,
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
}

#[derive(Debug, Args)]
pub struct PlayArgs {
    #[command(flatten)]
    pub server: ServerArgs,
    #[command(flatten)]
    pub playback: PlaybackArgs,
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
        assert_eq!(secret("PSH_TEST_UNSET_VAR", Some(&file), "--x").unwrap(), "ends in a space ");
        std::fs::write(&file, "\n").unwrap();
        assert!(secret("PSH_TEST_UNSET_VAR", Some(&file), "--x").is_err());
        assert!(secret("PSH_TEST_UNSET_VAR", None, "--x").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
