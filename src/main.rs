//! psysonic-headless: a headless Psysonic player for a Navidrome server, controlled through a Psysonic Hub.

mod audio;
mod config;
mod player;
mod subsonic;

use std::time::Duration;

use anyhow::{Result, bail};
use clap::Parser;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing_subscriber::EnvFilter;

use config::{Cli, Command, PlayArgs};
use player::{Command as PlayerCommand, Controller, Repeat};
use subsonic::{Quality, Subsonic};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .init();

    match Cli::parse().command {
        Command::Devices => list_devices(),
        Command::Play(args) => play(args).await,
    }
}

fn list_devices() -> Result<()> {
    for d in audio::list_devices()? {
        println!(
            "{}{}\n    id: {}",
            d.name,
            if d.default { "  (default)" } else { "" },
            d.id
        );
    }
    Ok(())
}

async fn play(args: PlayArgs) -> Result<()> {
    let quality = Quality {
        max_bitrate: args.playback.max_bitrate,
        format: args.playback.format,
    };
    let client = Subsonic::new(
        &args.server.server_url,
        &args.server.server_user,
        &args.server.password()?,
        quality,
    )?;
    client.ping().await?;

    let songs = if let Some(id) = &args.album {
        client.album_songs(id).await?
    } else if let Some(id) = &args.playlist {
        client.playlist_songs(id).await?
    } else {
        let mut songs = Vec::new();
        for id in &args.song {
            songs.push(client.song(id).await?);
        }
        songs
    };
    if songs.is_empty() {
        bail!("nothing to play: give --album, --playlist or --song");
    }

    let volume = f32::from(args.playback.volume) / 100.0;
    let (commands, status) = Controller::spawn(
        client,
        args.playback.audio_device.clone(),
        volume,
        args.playback.replay_gain,
    );
    commands.send(PlayerCommand::Load { songs, start: 0 })?;

    // What plays, printed whenever it changes.
    let mut watch = status.clone();
    tokio::spawn(async move {
        let mut last = (String::new(), false, None::<String>);
        while watch.changed().await.is_ok() {
            let s = watch.borrow().clone();
            let id = s.song.as_ref().map(|x| x.id.clone()).unwrap_or_default();
            if (id.clone(), s.playing, s.error.clone()) == last {
                continue;
            }
            last = (id, s.playing, s.error.clone());
            match (&s.song, &s.error) {
                (_, Some(e)) => println!("! {e}"),
                (Some(song), None) => println!(
                    "{} {}/{}  {} – {}  [vol {:.0}%, repeat {:?}]",
                    if s.playing { "▶" } else { "⏸" },
                    s.index + 1,
                    s.total,
                    song.artist.as_deref().unwrap_or("?"),
                    song.title,
                    s.volume * 100.0,
                    s.repeat,
                ),
                (None, None) => println!("■ stopped"),
            }
        }
    });

    println!(
        "keys: space/p = pause, n = next, b = previous, f/r = +/-10 s, + / - = volume, 1/a/o = repeat one/all/off, q = quit (then Enter)"
    );
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { break };
                let position = status.borrow().position;
                let volume = status.borrow().volume;
                let command = match line.trim() {
                    "" | "p" => PlayerCommand::Toggle,
                    "n" => PlayerCommand::Next,
                    "b" => PlayerCommand::Previous,
                    "f" => PlayerCommand::Seek(position + Duration::from_secs(10)),
                    "r" => PlayerCommand::Seek(position.saturating_sub(Duration::from_secs(10))),
                    "+" => PlayerCommand::Volume(volume + 0.05),
                    "-" => PlayerCommand::Volume(volume - 0.05),
                    "1" => PlayerCommand::SetRepeat(Repeat::One),
                    "a" => PlayerCommand::SetRepeat(Repeat::All),
                    "o" => PlayerCommand::SetRepeat(Repeat::Off),
                    "q" => break,
                    other => {
                        println!("unknown key: {other}");
                        continue;
                    }
                };
                commands.send(command)?;
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    Ok(())
}
