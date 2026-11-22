# Progress

## Done

- **Milestone 1, playback:** Navidrome / Subsonic client (salted-token auth, Psysonic's stream quality and format options), audio engine (Symphonia decode, gapless back-to-back playback, output reopened at each track's sample rate), ReplayGain modes like Psysonic's, `play` command with keyboard controls, `devices` command.
- **Milestone 2, hub player:** `run` command joins a Psysonic Hub as a player: hello with Psysonic-compatible server keys, state and queue window reporting, cover upload, all protocol commands (play, pause, toggle, next, prev, seek, volume, shuffle, repeat, shuffleQueue, shuffleUpcomingQueue, playIndex, playItem), yield (pause, save the queue on the server, confirm) and takeover (load the saved queue, resume at the hub's position), following the shared queue while idle, reconnect with backoff, SIGTERM shutdown.
- Checked end to end against a local hub and a fake Subsonic server: playback in real time with gapless changes, control from the hub web page, cover on the page, handoff between two headless players.
- **Streaming and every decoder:** playback starts once the first bytes arrive (a 90 MB hi-res FLAC took 13 s before); decoding ported from Psysonic with Symphonia 0.6 and every format plus Opus through bundled libopus, so Navidrome's transcodes (Opus/Ogg at a bitrate cap) play. Skipping to the already-loaded next track reuses it. Checked against the real Navidrome: a 320 kbps Opus transcode started in 60 ms.
- **Milestone 3, library:** answers the hub's library lookups (albums by sort with paging, artists, playlists, search, an album, an artist, a playlist) and artwork requests, with the desktop's page sizes and fields. Checked against the real Navidrome through a local hub with only this player on: every lookup answered in 10 to 80 ms, artwork in under 10 ms.
- **Ranged reads and starts at a position:** seeks and starts ahead of the download fetch that part with a Range request, and Ogg/AIFF stay seekable while probing (before, every seek on Navidrome's Opus transcodes failed and a handoff started at 0:00). A track starting at a position is positioned before it reaches the output. Checked with a handoff through a local hub into this player at 2:00 of a 320 kbps Opus stream: it resumed at 2:00, 0.3 s after the transfer.
- **Track cache** (`--cache original|streamed|off`, 256 MB by default): prefetches the next five tracks within the size limit and plays them from disk, with the desktop's rules for originals. Checked against the real Navidrome at a 320 kbps cap: the next two hi-res FLAC originals (90 and 103 MB) were prefetched while the first track streamed, and skipping to them started in 0 and 52 ms.
- **Milestone 5, packaging:** `curl | sh` installer, `install` / `update` / `uninstall`, systemd service as the `psysonic-headless` user, releases by semantic-release with native x86_64 and aarch64 builds. `tests/installer.sh` checks it on Debian 13 and Ubuntu 22.04 in Docker.

## Known issues

- Not yet tried on a real DAC, or in a handoff with the Psysonic desktop app.
- A queue edit in the last 10 seconds of a track can't change the next track any more (it is already queued in the engine).

## Next

- Milestone 4: scrobbling.
- Milestone 6: settings managed from the hub.
