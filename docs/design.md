# psysonic-headless design

## Goal

A headless music player for Linux boxes without a screen (a Raspberry Pi 4 or 5 with a DAC hat in a living room). It plays from a Navidrome / Subsonic server and is controlled entirely through a [Psysonic Hub](https://github.com/ImAsra/psysonic-hub): the hub's web remote, the Psysonic desktop app and anything else that speaks the hub protocol see it as one more player.

It is a separate project derived from [Psysonic](https://github.com/Psysonic/psysonic), under the same licence (GPL-3.0-or-later), and follows its ideals: faithful, high-fidelity playback; open source; the desktop app stays the primary client. If the Psysonic project wants to adopt it under its organisation, it can.

## Principles

- **Hi-fi by default.** Original files unless a quality cap is set; the output opens at each track's own sample rate when the device supports it (no resampling); volume 100%; no processing that is not asked for.
- **Same behaviour as Psysonic** where they overlap: stream quality and format options, ReplayGain modes and defaults, queue handoff through the server's saved play queue.
- **No screen, no state on the box beyond its config.** Everything a user changes day to day is changed from the hub.

## Configuration

Every option is a command-line flag that can also be given as an environment variable (`--hub-url` / `PSYSONIC_HUB_URL`), so the same binary runs by hand or as a systemd service with an `EnvironmentFile`.

Secrets (hub token, server password) are read from an environment variable or a file (`--hub-token-file`, `--server-password-file`), never from a flag value, because command lines are visible to every user on the box.

| Option | Meaning |
|---|---|
| `--server-url`, `--server-user`, `--server-password[-file]` | The Navidrome / Subsonic server and account |
| `--hub-url`, `--hub-token[-file]` | The hub to join |
| `--device-name` | How the player appears on the hub |
| `--audio-device` | Output device (default: the system default) |
| `--max-bitrate` | `0` (original) or 320, 256, 192, 128, 96, 64 kbps, like Psysonic |
| `--format` | `auto`, `mp3`, `opus`, `aac`: transcode format when capped, like Psysonic |
| `--replay-gain` | `off` (default), `track`, `album`, `auto`, like Psysonic |
| `--cache` | `original` (default): keep the original files of upcoming tracks, played whatever the bitrate cap, like Psysonic; `streamed`: keep tracks as streamed with the settings above; `off` |
| `--cache-max-mb` | Cache size, default 256 MB (0 = off) |
| `--cache-dir` | Default: systemd's `CACHE_DIRECTORY`, `$XDG_CACHE_HOME` or `~/.cache`, then `psysonic-headless` |
| `--scrobble` | `on` (default) or `off` |
| `--scrobble-percent` | How much of a track has to play before it counts: 25 to 90, default 50, like Psysonic |
| `--lastfm-session-file` | The Last.fm session key (from `lastfm-login` or `install --lastfm`); without it, plays go to the server only |

Once settings are managed by the hub, the hub's values replace these defaults; the server and hub connection always come from the config.

## Architecture

One binary, one process. Modules:

- `config`: flags and environment variables.
- `subsonic`: the server client: authentication (salted token), stream URLs with quality options, albums, playlists, songs, the saved play queue, scrobbling and library lookups.
- `audio`: the engine. Tracks are streamed: playback starts once the beginning of the file has arrived while the rest downloads. A read far ahead of the download (a seek, a start at a position, the Ogg demuxer reading the end of the file) is fetched on its own with an HTTP Range request, followed by read-ahead windows, when the server takes ranges and gives the size; otherwise it waits for the download. A track that starts at a position (a handoff, a resume) is positioned before it reaches the output, so nothing plays from elsewhere first. Decoding is Psysonic's: Symphonia 0.6 with every format it supports (FLAC, MP3, AAC/M4A, ALAC, Ogg Vorbis, WAV, AIFF, PCM, ADPCM) plus Opus through libopus (bundled), with gapless trimming where the container reports the encoder gap. A rodio `Player` plays tracks back to back: the next track is loaded early and handed to the player in the last seconds of the current one (gapless), so queue edits until then still decide what follows. The output device is reopened when a track's sample rate differs from the open one. ReplayGain is applied as a fixed gain per track.
- `cache`: the track cache on disk, one folder per server and user. The next five tracks are prefetched (the next always, the ones after while they fit). `original` mode fetches originals with `format=raw` and keeps one only if its size is the original's; a played stream is kept too when it is the original (no cap). `streamed` mode fetches and keeps streams at the current settings, named by bitrate and format, so a copy at other settings never stands in. Files are written as `.part` and renamed once their size matches. When full, the least recently played go first, never the track playing or the next one. A track in the cache plays from it.
- `player`: the queue (order, current position, shuffle, repeat) and the controller that drives the engine, preloads the next track and reports state.
- `scrobble`: like Psysonic. When a track starts playing it is reported as now playing; a play counts once the set share of it has played, timed from when it would have started at 0:00. A track taken over past that point isn't counted again (the device it came from counted it); the same track again (repeat) is a new play. Plays go to the server (`scrobble`, which keeps play counts and recently played) and, when signed in, straight to Last.fm (`track.updateNowPlaying`, `track.scrobble` with the main artist). Last.fm plays that fail on an outage or a revoked session are kept in memory and retried after 1, 2, 4 … 60 minutes, at most 500 and for 14 days. Last.fm sign-in is Psysonic's token flow: the app's key and secret are built into release builds, the player shows the link where the listener allows access, and keeps the session key it then gets.
- `hub`: the hub client, protocol v1: a player device that reports its state and queue window, obeys commands, and takes part in handoffs through the server's saved play queue. It pings the hub every 20 s and reconnects when nothing has come back for 45 s, so a connection that died during sleep or a network change is replaced.
- `hub::library`: answers the hub's library lookups and artwork requests from the server, with the same page sizes and fields as the Psysonic desktop.

## Installation

- Releases are cut by semantic-release from `main` (`feat` = minor, `fix` = patch); the version lives in git tags and is built into the binary. Each release carries `psysonic-headless-linux-x86_64` and `-aarch64` (native builds on Ubuntu 22.04: glibc 2.35 or newer, the system's ALSA library), `SHA256SUMS`, `install.sh` and `VERSION`.
- `install.sh` stays thin: it detects the architecture, installs the ALSA library if missing, downloads the binary, checks it against `SHA256SUMS` and runs `sudo psysonic-headless install` with its arguments.
- `psysonic-headless install` asks for what is missing (from the terminal, also through `curl | sh`; secrets without echo) or takes flags, checks the server sign-in, and writes `/etc/psysonic-headless/config.env` (the unit's environment file). The password and hub token go to `server-password` and `hub-token` there, owned by root and readable by the service's group only (0640). The first install also asks whether to scrobble to Last.fm and, if so, signs in (`--lastfm` does it later or again, `--no-lastfm` stops it); the session key goes to `lastfm-session` there, like the other secrets. It creates the `psysonic-headless` system user in the `audio` group, installs the binary to `/usr/local/bin` and the unit, and starts it. Running it again changes the given settings and keeps the rest; it never asks again for what is set.
- The unit runs `psysonic-headless run` as that user with systemd's cache directory (`/var/cache/psysonic-headless`) for the track cache, and restarts it if it stops.
- `update` compares the latest release's `VERSION` with its own, downloads the binary, checks it and runs its `install`. `uninstall` removes the unit and binary and keeps the settings and cache; `--purge` removes those and the user too. `PSYSONIC_DOWNLOAD` points both at another release folder (a URL or a directory).

## Roadmap

1. Play an album, playlist or songs from the server to the output device from the command line, gapless, with ReplayGain and quality options.
2. Join the hub as a player: state, commands, queue window, handoff with Psysonic.
3. Answer library and artwork lookups, so the web remote can browse with only this player on.
4. Scrobbling.
5. Packaging: one-line installer, systemd service, `update`, x86_64 and aarch64 releases.
6. Settings managed from the hub: the player publishes its settings and the web remote edits them.

## Open questions

- A downloaded track is held in memory until it is done playing (two tracks at most).
- Last.fm plays not sent yet are lost when the player restarts.
