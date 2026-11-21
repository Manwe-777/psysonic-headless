# psysonic-headless

[![CI](https://github.com/Manwe-777/psysonic-headless/actions/workflows/ci.yml/badge.svg)](https://github.com/Manwe-777/psysonic-headless/actions/workflows/ci.yml)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)

A headless music player for Linux boxes without a screen, such as a Raspberry Pi with a DAC. It plays from a Navidrome or Subsonic server and is controlled through a [Psysonic Hub](https://github.com/ImAsra/psysonic-hub).

Derived from [Psysonic](https://github.com/Psysonic/psysonic) and following its ideals. The Psysonic project is welcome to adopt it.

## Run

```sh
psysonic-headless devices      # list output devices
PSYSONIC_SERVER_PASSWORD=... psysonic-headless play \
  --server-url https://music.example.com --server-user me --album <album id>
```

Every option can also be set as an environment variable; see `psysonic-headless play --help`.

## Test

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

## Build

Needs the ALSA development files (`libasound2-dev` on Debian and Raspberry Pi OS).

```sh
cargo build --release
```
