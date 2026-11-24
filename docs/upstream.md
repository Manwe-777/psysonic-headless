# Following upstream

psysonic-headless reimplements parts of the Psysonic desktop (Rust and TypeScript) and of the Psysonic Hub. It does not share their code at build time, so their changes are ported by hand. `upstream/` records what is followed:

- `upstream/sources.tsv`: each upstream (name, git URL, branch) and the commit it was last synced with.
  - `psysonic`: Psysonic/psysonic `main`, for decoding, streaming, ReplayGain, the track cache, the Subsonic client and scrobbling.
  - `hub-client`: ImAsra/psysonic `feat/hub-poc`, for the desktop's hub client until it reaches Psysonic `main`.
  - `hub`: ImAsra/psysonic-hub `main`, for the hub protocol.
- `upstream/files.tsv`: which headless file follows which upstream file or folder.

## Commands

```sh
scripts/upstream.sh status                    # changes per source since the last sync (exit 10 when any)
scripts/upstream.sh diff hub-client           # the upstream diff of one source
scripts/upstream.sh diff src/hub/client.rs    # only what one headless file follows
scripts/upstream.sh mark hub-client [commit]  # record a source as synced (default: its current tip)
```

Upstreams are kept as blobless bare clones in `~/.cache/psysonic-headless/upstream` (`UPSTREAM_CACHE` to change).

## Porting

1. Read the diff for the headless file and port what applies.
2. Run `scripts/upstream.sh mark <source>` and commit `upstream/sources.tsv` with the port.
3. When an upstream file is renamed or a new one becomes relevant, update `upstream/files.tsv`.

## Weekly check

`.github/workflows/upstream.yml` runs `status` every Monday and on demand. While anything is unsynced, one open issue labelled `upstream` lists it, with a comment when new upstream commits appear. The issue closes once every source is synced.
