---
title: "Archiving a channel"
description: "Capture a whole channel's uploads as a catalog or a playable vault, widen it with shorts, live streams, playlists, and community posts, and bound it by time and count."
weight: 10
---

Archiving a channel is kura's headline job.
Point it at a channel and it captures the channel record plus all of its uploads, each video's metadata, thumbnail, transcript, and chapters.

## Pointing at a channel

kura accepts the same channel grammar `ytb` accepts: a `@handle`, a `UC...` channel id, or a `/c/` or `/user/` vanity path.

```bash
kura archive @mkbhd
kura archive UCBJycsmduvYEL83R_U4JriQ
```

The capture is bounded by `--max`, which defaults to 1000 for a channel so a bare `kura archive @channel` does not try to pull a whole back catalog by accident.
Raise it to go further, or set `--max 0` for everything the surface gives:

```bash
kura archive @mkbhd --max 3000
kura archive @mkbhd --max 0
```

## Catalog or vault

At the default `--depth meta` a channel capture is a catalog: records, thumbnails, transcripts, and chapters, but no stream bytes.
A 1500-video channel is a few hundred megabytes of JSON, thumbnails, and text that you can read, grep, and browse offline.

Add `--depth media` to make it a playable vault, downloading the video and audio streams via the native engine:

```bash
kura archive @mkbhd --depth media -f bv*+ba/b
```

See [depth and streams](/guides/depth-and-streams/) for the full model and the format selectors.

## Widening the capture

By default a channel capture takes the uploads tab.
Four flags widen it to the other tabs:

```bash
kura archive @mkbhd --shorts --streams --playlists --community
```

- `--shorts` adds the channel's Shorts.
- `--streams` adds its past live streams.
- `--playlists` captures the channel's playlists and their video order.
- `--community` captures community posts.

## Bounding by time

`--since`, `--until`, and `--since-id` bound which uploads the capture covers:

```bash
# Just 2025 onward
kura archive @mkbhd --since 2025-01-01

# A fixed span
kura archive @mkbhd --since 2024-01-01 --until 2025-01-01
```

Both `--since` and `--until` accept a bare calendar date (`2006-01-02`, read as UTC midnight) or a full RFC3339 timestamp.
`--since-id` sets the floor by video id, which is what an incremental `kura add` uses under the hood.

## Sidecars

Add comment and SponsorBlock sidecars per video:

```bash
kura archive @mkbhd --comments --sponsorblock
```

`--comments` takes `--max-comments` and `--sort top|new`.
From a flagged IP YouTube hides comments and serves an empty transcript; kura records the gap in the manifest and the rest of the capture proceeds.
See the [introduction](/getting-started/introduction/) for what is and is not reachable from a gated IP.

## Next

- Other target kinds: [capturing videos and playlists](/guides/capturing-videos-and-playlists/).
- Choosing how deep to go: [depth and streams](/guides/depth-and-streams/).
- Making the spoken content searchable: [transcripts and search](/guides/transcripts-and-search/).
- Keeping the archive current: [incremental and resumable captures](/guides/incremental-and-resumable/).
