---
title: "Introduction"
description: "How kura captures YouTube content through the free ytb-cli engine, why JSON is the source of truth, and what the capture depth decides."
weight: 10
---

A YouTube video you watch in your browser is not a document, it is the output of a program.
The HTML YouTube sends is a near-empty shell, and the watch page is assembled by JavaScript fetching data and building the DOM.
That is why "Save As" fails: you keep the shell, not the video, and what you do keep still calls home when you open it.
Worse, the content itself is fragile.
Videos get deleted, channels go private, and the feed only exposes a recent window.

kura treats an archive as a capture, a store, and a set of views, in that order, with one extra axis on top: how deep to go.

## 1. Capture through the free engine

kura reuses the ytb-cli engine to read YouTube for free, with no API key, no login, and no cost.
It never invents its own scraping: it asks the engine for a video, a channel's uploads, a playlist, a search, and streams the records back.
The engine reads the same free InnerTube surfaces `ytb` reads (the watch-page data, the browse, next, player, and search endpoints), so kura gets structured videos, not a screenshot of a rendered page.

Two surfaces are gated not by auth but by the egress IP.
From a flagged IP YouTube hides comments and serves an empty transcript body.
kura detects the gap, records it honestly in the manifest, falls back to the optional `yt-dlp` path for transcript text when present, and exits with the documented code rather than writing a half-empty archive and calling it done.
A clean IP gets all of it.

## 2. Store JSON as the source of truth

Every video is written to `videos/<id>.json` the moment it arrives, with the untouched upstream payload beside it as `videos/<id>.raw.json`.
This is the canonical record, and it is what every other part of the archive is derived from.
Writing each record immediately is deliberate: a run that is interrupted or rate-limited still leaves a valid, smaller archive on disk.
The path is a pure function of the 11-character video id, so a re-capture overwrites the same file and the output stays deterministic.

The transcript is stored both as timed `.vtt` and flat `.txt`, so the archive is greppable for the spoken word.
The comments, chapters, and SponsorBlock segments are first-class stored records too, not render-time afterthoughts.

## 3. Choose the depth

Depth decides whether and how kura saves the bytes of a video, and it is the YouTube-specific decision a tweet archive never has to make.
A video's "media" is not a photo, it is a multi-hundred-megabyte stream behind a signature cipher and an nsig throttle.

- `--depth meta` (the default) keeps records, thumbnails, transcripts, chapters, and comments, but no stream bytes.
  Fast and small: a catalog of a channel.
- `--depth media` keeps everything `meta` keeps plus the video and audio streams, pulled by the native engine.
  The true offline vault that plays every video.
- `--depth audio` keeps records plus the audio stream only, for music, lectures, and podcasts.

A catalog can be upgraded to a vault later with `kura add --depth media` and no re-fetch of the records it already holds.
See [depth and streams](/guides/depth-and-streams/) for the full model.

## 4. Derive the views

From the stored JSON, kura renders inert views you can read with no network:

- **HTML.** A browsable `index.html`, and per-video watch pages under `html/`.
  Scripts and handlers are stripped, media points at the local files, and links resolve to the other pages in the archive.
  At media depth the watch page carries a passive HTML5 `<video>` element over the local file.
- **Markdown.** A `README.md` index plus per-video Markdown under `md/`, with the full transcript inline, for reading in any editor or feeding to other tools.

Because the views are derived, they are disposable.
`kura render <repo>` rebuilds them from the JSON with no network, which is how you replay a renderer improvement over an old archive or add a Markdown view to an HTML-only one.

## The shape of an archive

A capture lands in a self-contained repository at `<out>/youtube/<root>`, where `<out>` defaults to `$HOME/data/kura` (or `$KURA_OUT`).
The root is the canonical, case-stable target identity: a channel keeps its `@handle`, while a video, playlist, and search are prefixed by kind and lowercased (`video-dqw4w9wgxcq`, `playlist-plxxxx`, `search-lofi-mix`).
Move the folder anywhere and it still opens.
See [repository layout](/reference/repository-layout/) for the full tree.

Next: [install kura](/getting-started/installation/).
