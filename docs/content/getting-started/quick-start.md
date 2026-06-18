---
title: "Quick start"
description: "From an empty terminal to a self-contained offline YouTube archive you can click through and play."
weight: 30
---

This walks the core loop: capture a channel, look at what landed on disk, serve it back, then deepen the capture to a playable vault and keep it up to date.

## 1. Capture a channel

```bash
kura archive @mkbhd
```

kura resolves the channel, streams its uploads through the free InnerTube surface, writes each video as JSON, downloads the thumbnails and transcripts beside it, and renders the HTML and Markdown views.
At the default `meta` depth it captures no stream bytes, so this is a fast, small catalog.
The summary tells you where the archive landed:

```
@mkbhd
  repo:        /home/you/data/kura/youtube/@mkbhd
  depth:       meta
  videos:      1480 total (+1480 new)
  transcripts: 1322
  range:       2009-03-21T... … 2026-06-17T...
  media:       1480 thumbs
```

## 2. Look at what landed

```bash
ls $HOME/data/kura/youtube/@mkbhd
```

```
videos/        # videos/<id>.json, the source of truth, plus sidecars
html/          # per-video inert watch pages
md/            # per-video Markdown with the inline transcript
media/         # localised thumbnails, avatars, banners (and streams at media depth)
_assets/       # kura.css
index.html     # the browsable archive home
README.md      # the Markdown index
channel.json   # the captured channel
manifest.json  # counts, depth, range, capture history, gaps
```

Open `index.html` directly in a browser and it renders offline, with no network.

## 3. Serve it back

`kura serve` runs a local static server so links, media, and the `<video>` range requests resolve exactly as they would on a real host:

```bash
kura serve $HOME/data/kura/youtube/@mkbhd
# open http://127.0.0.1:8080
```

## 4. Go deeper, then keep it fresh

A meta catalog reads and greps but does not play.
Upgrade it to a vault: `kura add --depth media` fetches only the streams for the videos already on disk, merging separate video and audio with ffmpeg when it is present:

```bash
kura add @mkbhd --depth media
```

Now the watch pages carry a passive HTML5 `<video>` element over the local file and play offline.

Later, re-run with `kura add` to fetch only the uploads that are new since the last capture and re-render just the affected pages:

```bash
kura add @mkbhd
```

## Where to go next

- The [guides](/guides/) cover archiving a channel, capturing videos and playlists, the depth and stream model, the transcript corpus, incremental re-capture, and media and views in depth.
- The [CLI reference](/reference/cli/) lists every command and flag.
