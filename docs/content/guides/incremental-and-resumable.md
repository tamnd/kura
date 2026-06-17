---
title: "Incremental and resumable captures"
description: "Re-run a capture to fetch only what is new, resume an interrupted run or a half-downloaded video, upgrade a catalog to a vault, force a clean recapture, and pin the stamp for reproducible output."
weight: 50
---

A kura archive is meant to be kept, not captured once and forgotten. Re-running is
cheap because kura knows what it already holds.

## Fetching only what is new

`kura add` (alias `kura update`) re-captures an existing target but fetches only
what is newer than the newest video already on disk, then re-renders only the
affected pages:

```bash
kura add @mkbhd
```

Under the hood, an add reads the newest captured upload id from the manifest and
seeds the uploads reader with it, so it pulls only the gap since your last capture
and nothing more. The capture summary shows the delta as `(+N new)`. `kura
archive` on an existing repo does the same incremental fetch; `add` is just the
clearer name for a re-run.

For a playlist, the merge re-reads the list and adds only video ids not already
stored. An already-held video is refreshed in place (its counts update via
overwrite) without duplicating files.

## Upgrading a catalog to a vault

A meta catalog and a media vault differ only in the stream files. `kura add
--depth media` over a meta repo fetches only the streams for videos whose records
are already present, upgrading the catalog to a playable vault without re-fetching
anything else:

```bash
kura add @mkbhd --depth media
```

## Resuming an interrupted run

Every record is written to disk the instant it arrives, not buffered for the end.
That means an interrupted run is never wasted:

- Press Ctrl-C and kura stops, keeping every record it already wrote. It exits
  with code 130.
- Hit a rate limit on a long run and kura exits 5 with the partial archive intact.

Either way, run the same command again (or `kura add`) and it continues from what
is on disk, fetching only the rest. `--resume` is on by default.

Distinct to kura, a stream download is resumable too. The ranged fetcher's offset
state lives in `state.json`, so a capture interrupted mid-stream resumes a
half-downloaded video from its offset rather than restarting the file.

## Forcing a clean recapture

To ignore the held state and recapture from scratch (for example, to pull in
updated counts on videos you already have), use `--force`. Records and views
overwrite; cached media still de-dupes:

```bash
kura add @mkbhd --force
```

## Reproducible output

kura's output is deterministic by design: record paths and media filenames are
pure functions of their content, the manifest's record-bearing fields are sorted
by id, and transcript offsets come from the stored `.vtt`. The one wall-clock
value is the capture stamp. Pin it with `--date` to make a run byte-for-byte
reproducible:

```bash
kura archive @mkbhd --date 2026-06-17T00:00:00Z
```

The same `--date` is available on `kura render` to fix the footer stamp when you
rebuild the views.

## Previewing without fetching

`--dry-run` prints the capture plan and the requests it would make without
touching the network:

```bash
kura archive @mkbhd --dry-run
```

## A self-contained, movable archive

The repository is fully self-contained. Records, media, views, CSS, and the
manifest all live under the one root directory, with every internal reference
written as a relative path. Move the folder to another disk or machine, open
`index.html`, and it still works with the network unplugged. To browse it over
HTTP so the `<video>` range requests work, point `kura serve` at it:

```bash
kura serve $HOME/data/kura/youtube/@mkbhd
```

## Next

- The [CLI reference](/reference/cli/) lists every flag.
- [Repository layout](/reference/repository-layout/) maps what lands on disk.
