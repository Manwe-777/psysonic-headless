//! psysonic-headless: a headless Psysonic player for a Navidrome server, controlled through a Psysonic Hub.

mod audio;
mod config;
mod hub;
mod player;
mod subsonic;

use std::time::Duration;

use anyhow::{Result, bail};
use clap::Parser;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing_subscriber::EnvFilter;

use config::{Cli, Command, PlayArgs, RunArgs};
use player::{Command as PlayerCommand, Controller, Repeat};
use subsonic::{Quality, Subsonic};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .init();

    match Cli::parse().command {
        Command::Run(args) => run(args).await,
        Command::Devices => list_devices(),
        Command::Play(args) => play(args).await,
    }
}

/// The service mode: a player on the hub until stopped.
async fn run(args: RunArgs) -> Result<()> {
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

    let host = hostname();
    let name = args.hub.device_name.clone().unwrap_or_else(|| host.clone());
    let device_id = args.hub.device_id.clone().unwrap_or_else(|| {
        let key = hub::server_key(&host, &name); // any stable hash of host + name will do
        format!("headless-{}", &key[..12])
    });
    let config = hub::HubConfig {
        url: args.hub.hub_url.clone(),
        token: args.hub.token()?,
        device_id,
        name,
        server_keys: vec![hub::server_key(&args.server.server_url, &args.server.server_user)],
    };

    let volume = f32::from(args.playback.volume) / 100.0;
    let (commands, status) = Controller::spawn(
        client.clone(),
        args.playback.audio_device,
        volume,
        args.playback.replay_gain,
    );
    tokio::select! {
        () = hub::run(config, client, commands, status) => {}
        _ = shutdown_signal() => tracing::info!("stopping"),
    }
    Ok(())
}

/// Ctrl+C, or SIGTERM from systemd.
async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|h| h.trim().to_string())
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "psysonic-headless".into())
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
    commands.send(PlayerCommand::Load {
        songs,
        start: 0,
        position: Duration::ZERO,
        play: true,
    })?;

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
                    s.queue.len(),
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
