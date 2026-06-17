---
title: "Capturing videos and playlists"
description: "Archive a single video, a playlist in order, a search query, or a music album, each into its own self-contained repository."
weight: 20
---

A channel is just one kind of target. kura captures several others, each into its
own self-contained repository.

## A single video

Pass a video id or any watch URL. No setup is needed for a public video:

```bash
kura archive dQw4w9WgXcQ
kura archive https://www.youtube.com/watch?v=dQw4w9WgXcQ
kura archive https://youtu.be/dQw4w9WgXcQ
```

The id grammar matches `ytb`: the 11-character id, `watch?v=`, `youtu.be`,
`/shorts/`, and `/embed/` URLs all resolve to the same video. A single-video
capture is unbounded by `--max`: it is just the one video and its sidecars.

Add the playable stream with `--depth media`:

```bash
kura archive dQw4w9WgXcQ --depth media
```

## A playlist

Pass a `PL...` or `UU...` playlist id and kura captures the playlist record plus
every video in it, in order:

```bash
kura archive PLxxxxxxxxxxxxxxxx
```

A playlist capture is unbounded too: it takes the whole list. The `playlists/`
folder records the playlist and its video-id order, so the rendered home lists the
videos as the playlist orders them rather than by date.

## A search

`--search` archives a search instead of a channel. Quote the query:

```bash
kura archive --search "lofi mix" --max 200
```

Search is paged, so bound the result count with `--max` (it defaults to 1000 for
a search, like a channel).

## A music album

`--album` captures a music album and its tracks. Pair it with `--depth audio` for
an offline audio archive:

```bash
kura archive --album <id> --depth audio -x
```

## Where each target lands

Each target writes one self-contained repository under `<out>/youtube/<root>`:

| Target | Root |
|--------|------|
| Video `dQw4w9WgXcQ` | `video-dqw4w9wgxcq` |
| Channel `@MKBHD` | `@mkbhd` |
| Playlist `PLxxxx` | `playlist-plxxxx` |
| Search `lofi mix` | `search-lofi-mix` |
| Album `<id>` | the lowercased album id |

Two captures of the same target land in the same repo and merge. See
[repository layout](/reference/repository-layout/) for the full tree.

## Next

- Choosing how deep to go: [depth and streams](/guides/depth-and-streams/).
- The greppable transcript corpus: [transcripts and search](/guides/transcripts-and-search/).
- Keeping it current: [incremental and resumable captures](/guides/incremental-and-resumable/).
