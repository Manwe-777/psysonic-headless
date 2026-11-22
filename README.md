# psysonic-headless

[![CI](https://github.com/Manwe-777/psysonic-headless/actions/workflows/ci.yml/badge.svg)](https://github.com/Manwe-777/psysonic-headless/actions/workflows/ci.yml)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)

A headless music player for Linux boxes without a screen, such as a Raspberry Pi with a DAC. It plays from a Navidrome or Subsonic server and is controlled through a [Psysonic Hub](https://github.com/ImAsra/psysonic-hub).

Derived from [Psysonic](https://github.com/Psysonic/psysonic) and following its ideals. The Psysonic project is welcome to adopt it.

## Install

On Linux (x86_64, or a Raspberry Pi with a 64-bit OS), as a systemd service:

```sh
curl -fsSL https://github.com/Manwe-777/psysonic-headless/releases/latest/download/install.sh | sh
```

It asks for the server, user, password, hub address and hub device token (the hub's Settings page), and whether to scrobble to Last.fm: then it shows a link to open on any device and allow access. Plays are always counted on the server too. To change a setting, run `sudo psysonic-headless install --<setting> <value>`, for example `--audio-device` (from `psysonic-headless devices`) or `--max-bitrate 320`.

- Last.fm later, or sign in again: `sudo psysonic-headless install --lastfm` (`--no-lastfm` stops it)
- Update: `sudo psysonic-headless update` (`--check` only looks)
- Logs: `journalctl -u psysonic-headless -f`
- Remove: `sudo psysonic-headless uninstall` (`--purge` also deletes the settings and the track cache)

## Run

```sh
psysonic-headless devices      # list output devices
PSYSONIC_SERVER_PASSWORD=... psysonic-headless play \
  --server-url https://music.example.com --server-user me --album <album id>
```

`psysonic-headless run` joins a hub and plays what it says (what the service runs). For Last.fm, `psysonic-headless lastfm-login --session-file FILE` once, then `--lastfm-session-file FILE`. Every option can also be set as an environment variable; see `psysonic-headless run --help`.

## Test

```sh
cargo test
cargo clippy --all-targets -- -D warnings
tests/installer.sh      # the installer on Debian and Ubuntu, in Docker
```

## Build

Needs the ALSA development files (`libasound2-dev` on Debian and Raspberry Pi OS).

```sh
cargo build --release
```

Release builds have the app's Last.fm key built in (`PSYSONIC_HEADLESS_LASTFM_KEY` and `PSYSONIC_HEADLESS_LASTFM_SECRET` at build time, from the `LASTFM_API_KEY` and `LASTFM_API_SECRET` repository secrets). Other builds take one at run time from `PSYSONIC_LASTFM_API_KEY` and `PSYSONIC_LASTFM_API_SECRET_FILE`.
