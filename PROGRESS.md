# Progress

## Done

- **Milestone 1, playback:** Navidrome / Subsonic client (salted-token auth, Psysonic's stream quality and format options), audio engine (whole-file fetch, Symphonia decode via rodio, gapless back-to-back playback, output reopened at each track's sample rate), ReplayGain modes like Psysonic's, `play` command with keyboard controls, `devices` command.
- **Milestone 2, hub player:** `run` command joins a Psysonic Hub as a player: hello with Psysonic-compatible server keys, state and queue window reporting, cover upload, all protocol commands (play, pause, toggle, next, prev, seek, volume, shuffle, repeat, shuffleQueue, shuffleUpcomingQueue, playIndex, playItem), yield (pause, save the queue on the server, confirm) and takeover (load the saved queue, resume at the hub's position), following the shared queue while idle, reconnect with backoff, SIGTERM shutdown.
- Checked end to end against a local hub and a fake Subsonic server: playback in real time with gapless changes, control from the hub web page, cover on the page, handoff between two headless players.
- **Streaming and every decoder:** playback starts once the first bytes arrive (a 90 MB hi-res FLAC took 13 s before); decoding ported from Psysonic with Symphonia 0.6 and every format plus Opus through bundled libopus, so Navidrome's transcodes (Opus/Ogg at a bitrate cap) play. Skipping to the already-loaded next track reuses it. Checked against the real Navidrome: a 320 kbps Opus transcode started in 60 ms.

## Known issues

- Not yet tried on a real DAC, or in a handoff with the Psysonic desktop app.
- A seek past what has downloaded waits for those bytes (no ranged requests yet).
- A queue edit in the last 10 seconds of a track can't change the next track any more (it is already queued in the engine).
- The library card of the hub web page shows "The player did not answer" with only this player on (milestone 3).

## Next

- Milestone 3: answer the hub's library lookups and artwork requests.
- Milestone 4: scrobbling.
- Milestone 5: packaging (.deb, systemd unit, arm64 builds, release workflow).
- Milestone 6: settings managed from the hub.
