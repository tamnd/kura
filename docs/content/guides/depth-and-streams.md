---
title: "Depth and streams"
description: "The meta, media, and audio depth model, selecting a stream format with yt-dlp grammar, the optional ffmpeg merge, and how a missing stream degrades a video without aborting the run."
weight: 30
---

Depth is the axis that decides whether and how kura saves the bytes of a video.
It is orthogonal to the target: any target can be captured at any depth. This is
the YouTube-specific decision a tweet archive never has to make, because a video's
"media" is not a photo, it is a multi-hundred-megabyte stream behind a signature
cipher and an nsig throttle.

## The three depths

```bash
kura archive @mkbhd --depth meta    # records, thumbnails, transcripts, chapters (default)
kura archive @mkbhd --depth media   # ...plus the playable video and audio streams
kura archive @mkbhd --depth audio   # records plus the audio stream only
```

- `--depth meta` (the default) keeps records, thumbnails, transcripts, chapters,
  and, with `--comments`, comments. No stream bytes. Fast and small: a catalog you
  can read, grep, and browse offline.
- `--depth media` keeps everything `meta` keeps plus the video and audio streams,
  fetched by the native engine. Large: the true offline vault that plays every
  video in the HTML view.
- `--depth audio` keeps records plus the audio stream only, for music, lectures,
  and podcasts where the picture is incidental. It is what `-x` implies.

The depth is recorded in the manifest, so `kura info` says plainly whether a repo
is a catalog or a vault.

## Upgrading a catalog to a vault

A meta catalog can become a media vault later, cheaply, because the records are
already on disk. `kura add --depth media` fetches only the streams for videos it
already holds:

```bash
kura add @mkbhd --depth media
```

It does not re-fetch the records, the thumbnails, or the transcripts. See
[incremental and resumable captures](/guides/incremental-and-resumable/).

## Selecting a format

Stream selection is delegated to the native engine and uses `yt-dlp` grammar.
`-f`/`--format` picks the format; the default is `bv*+ba/b` when ffmpeg is present
(best video plus best audio, merged), else `b` (the best single muxed file):

```bash
kura archive dQw4w9WgXcQ --depth media -f bv*+ba/b
kura archive dQw4w9WgXcQ --depth media -f 137+140
```

`-x`/`--audio-only` selects the audio stream (and implies audio depth). `--quality`
caps the height. `--concurrent` sets the number of ranged download workers:

```bash
kura archive @lofi --depth audio -x
kura archive @mkbhd --depth media --quality 1080 --concurrent 4
```

## The ffmpeg merge

When the selection is a separate video and audio pair, the engine merges them
into a single `mp4` with ffmpeg. ffmpeg is never linked: it is an optional
external binary found via `--ffmpeg-bin`, the `YTB_FFMPEG_BIN` environment
variable, or your `PATH`. Without it, kura selects a muxed progressive format so
the binary stays `CGO_ENABLED=0` and a download still succeeds.

```bash
kura archive dQw4w9WgXcQ --depth media --ffmpeg-bin /usr/bin/ffmpeg
```

## When a stream cannot be fetched

A stream that is HLS or DASH only with no progressive fallback, or that fails the
cipher, is recorded as a gap in the manifest with its reason, and the video still
archives at meta depth. A missing stream degrades the vault to a catalog for that
one video; it never aborts the run.

For the cases the native engine declines, `--tool yt-dlp` delegates the stream
fetch to a user-provided `yt-dlp`. It is opt-in, never bundled, and not required:

```bash
kura archive dQw4w9WgXcQ --depth media --tool yt-dlp
```

## Next

- The greppable transcript corpus: [transcripts and search](/guides/transcripts-and-search/).
- What lands on disk and what renders: [media and views](/guides/media-and-views/).
