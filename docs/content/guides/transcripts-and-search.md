---
title: "Transcripts and search"
description: "Build a greppable spoken-word corpus from a channel's transcripts, capture transcripts only for a light archive, pick languages, and grep across an entire channel."
weight: 40
---

A YouTube archive's most valuable text is often not the description, it is the transcript: a channel's entire spoken content, made searchable on disk.
kura stores every video's transcript in two shapes so the archive is both faithful and greppable.

## How transcripts are stored

For each video and each captured language, kura writes two files under `videos/`:

- `<vid>.transcript.<lang>.vtt` is the timed transcript, the source.
- `<vid>.transcript.<lang>.txt` is the flat transcript text, grep-friendly.

The `.vtt` preserves the cue timings so the rendered watch page can show a timestamped, readable transcript block.
The `.txt` strips the timing so a plain text search across a channel just works.

## A transcripts-only corpus

For a light, fast archive of just the spoken word, `--transcripts-only` captures each video's transcript text and skips the heavier sidecars:

```bash
kura archive @lexfridman --transcripts-only
```

This is the cheapest way to make a whole channel full-text searchable.
It writes the records and the transcript files but no streams, so a large channel stays small.

## Picking languages

`--lang` selects which transcript language or languages to store.
Without it, kura stores the default track:

```bash
kura archive @mkbhd --lang en
kura archive @arte --lang en,fr,de
```

## Grepping the corpus

Because the flat transcript text is on disk, a channel's entire spoken content is searchable with the tools you already have:

```bash
grep -rl "the thing he said about" ~/data/kura/youtube/@lexfridman/videos/
grep -rn "neural network" ~/data/kura/youtube/@lexfridman/videos/*.txt
```

The Markdown view embeds the full transcript inline too, so `grep -r` over `md/` searches the readable archive the same way.
No other tool in the family gives you this for YouTube.

## When a transcript is gated

From a flagged egress IP, YouTube serves an empty transcript body (poToken gating).
kura detects the empty timedtext, records the gap in the manifest, and falls back to the optional `yt-dlp` path for transcript text when a `yt-dlp` binary is available.
A clean IP gets the transcript directly.
The archive is honest about exactly which transcripts it could not capture rather than silently dropping them.

## Next

- Keeping the archive current: [incremental and resumable captures](/guides/incremental-and-resumable/).
- What renders and how: [media and views](/guides/media-and-views/).
