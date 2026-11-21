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

Once settings are managed by the hub, the hub's values replace these defaults; the server and hub connection always come from the config.

## Architecture

One binary, one process. Modules:

- `config`: flags and environment variables.
- `subsonic`: the server client: authentication (salted token), stream URLs with quality options, albums, playlists, songs, the saved play queue, scrobbling and library lookups.
- `audio`: the engine. Tracks are fetched whole into memory and decoded with Symphonia (through rodio). A `Player` plays them back to back, so the next track is appended before the current one ends (gapless). The output device is reopened when a track's sample rate differs from the open one. ReplayGain is applied as a fixed gain per track.
- `player`: the queue (order, current position, shuffle, repeat) and the controller that drives the engine, preloads the next track and reports state.
- `hub`: the hub client, protocol v1: a player device that reports its state and queue window, obeys commands, and takes part in handoffs through the server's saved play queue.
- `library`: answers the hub's library lookups and artwork requests from the server.

## Roadmap

1. Play an album, playlist or songs from the server to the output device from the command line, gapless, with ReplayGain and quality options.
2. Join the hub as a player: state, commands, queue window, handoff with Psysonic.
3. Answer library and artwork lookups, so the web remote can browse with only this player on.
4. Scrobbling.
5. Packaging: a `.deb` with a systemd unit running as its own user in the `audio` group, arm64 and x86_64 builds.
6. Settings managed from the hub: the player publishes its settings and the web remote edits them.

## Open questions

- Opus: Symphonia has no Opus decoder; `--format opus` needs an Opus decoder (Psysonic uses `symphonia-adapter-libopus`).
- Tracks are fetched whole before they play; very long files start later than with Psysonic's ranged streaming.
