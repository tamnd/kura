---
title: "kura"
description: "kura (蔵, storehouse) builds offline, browsable archives of YouTube content from one pure-Go binary. Capture a video, channel, playlist, or search into canonical JSON with localised media, optional video and audio streams, and inert HTML and Markdown views that open with the network unplugged. No API key."
heroTitle: "A YouTube archive that outlives the upload"
heroLead: "kura reads YouTube through the free ytb-cli engine, writes every video as canonical JSON, downloads the transcript and thumbnails beside it, and renders inert HTML and Markdown you can open straight from disk. Ask for media depth and it pulls the playable video and audio streams too, in pure Go, with no headless browser and no mandatory yt-dlp."
heroPrimaryURL: "/getting-started/quick-start/"
heroPrimaryText: "Get started"
---

YouTube is a stream that churns. Videos get deleted, channels go private or vanish, and the recommendation feed only ever shows you a recent slice. "Save As" on a watch page gives you a dead shell: the markup is built by JavaScript at runtime, so you keep a page that renders blank and still phones home, never the video, never the transcript, never the comment tree. kura (蔵, "storehouse") takes the opposite approach. It captures the content through the free ytb-cli engine, stores it as plain JSON, and renders views that run no code.

Say you want to keep MKBHD's channel on a laptop with no wifi. One command captures the catalog; a second serves it back offline:

```bash
kura archive @mkbhd
kura serve $HOME/data/kura/youtube/@mkbhd
```

## What it does

- **Captures over the free InnerTube surface.** kura reuses the ytb-cli engine to read YouTube with no API key, no login, and no cost. It never opens a browser and never fights the SPA: it reads the same structured surfaces ytb-cli reads, so it gets real video records, not a screenshot.
- **Keeps JSON as the source of truth.** Every video lands as `videos/<id>.json`, with the untouched upstream payload beside it. The HTML and Markdown views are derived from it and regenerable offline with [`kura render`](/guides/media-and-views/).
- **Localises the media, and optionally the streams.** Thumbnails, avatars, and banners are downloaded beside the records at every depth. At [media or audio depth](/guides/depth-and-streams/) the native engine pulls the video and audio bytes too, so the archive plays offline.
- **Builds a greppable transcript corpus.** Every video's transcript is stored as timed `.vtt` and flat `.txt`, so [`grep -r`](/guides/transcripts-and-search/) over a channel searches its entire spoken content.
- **Stays incremental and resumable.** Re-run with [`kura add`](/guides/incremental-and-resumable/) to fetch only what is new and even resume a half-downloaded video. Ctrl-C keeps what it already got. The output is deterministic.

## Where to go next

- New here? Start with the [introduction](/getting-started/introduction/), then the [quick start](/getting-started/quick-start/).
- Want to install it? See [installation](/getting-started/installation/).
- Looking for a specific task? The [guides](/guides/) cover archiving a channel, capturing videos and playlists, the depth and stream model, the transcript corpus, incremental re-capture, and media and views.
- Need every flag? The [CLI reference](/reference/cli/) is the full surface, and [repository layout](/reference/repository-layout/) maps what lands on disk.
