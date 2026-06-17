---
title: "Repository layout"
description: "The on-disk shape of a kura archive: the directory tree, what each file is, and the manifest fields."
weight: 20
---

A capture writes one self-contained repository. Everything it produces, records,
sidecars, media, views, styling, and the manifest, lives under a single root, and
every internal reference is a relative path, so the folder is movable and opens
with no network.

## Where it lands

The root is `<out>/youtube/<root>`, where `<out>` is `-o/--out` (default
`$HOME/data/kura`, or `$KURA_OUT`) and `<root>` is the canonical target identity:

| Target | Root |
|--------|------|
| Channel `@mkbhd` | `@mkbhd` |
| Video `dQw4w9WgXcQ` | `dQw4w9WgXcQ` |
| Playlist `PLxxxx` | `PLxxxx` |
| Search `lofi mix` | `search-lofi-mix` |
| Album `<id>` | the album id |

A channel `@handle` is resolved to its `UC...` id internally and recorded in the
manifest. Two captures of the same target land in the same repo and merge.

## The tree

A channel capture of `@mkbhd` looks like this:

```
$HOME/data/kura/youtube/@mkbhd/
├── manifest.json               # the repository index: target, depth, counts, range, stamps, gaps
├── index.html                  # the browsable archive home, inert
├── README.md                   # the Markdown index
├── channel.json                # the captured channel record
├── videos/                     # canonical records, the source of truth, plus sidecars
│   ├── <vid>.json              # canonical youtube.Video JSON, one per video
│   ├── <vid>.raw.json          # the untouched upstream payload, beside it
│   ├── <vid>.comments.json     # captured comments (when --comments)
│   ├── <vid>.transcript.<lang>.vtt   # the timed transcript
│   ├── <vid>.transcript.<lang>.txt   # the flat transcript, grep-friendly
│   ├── <vid>.chapters.json     # chapter list
│   └── <vid>.sponsorblock.json # SponsorBlock segments (when --sponsorblock)
├── html/                       # rendered inert per-video watch pages
│   └── <vid>.html
├── md/                         # rendered per-video Markdown with the inline transcript
│   └── <vid>.md
├── playlists/                  # captured playlist records and their video order
│   └── <plid>.json
├── community/                  # captured community posts (when --community)
│   └── <postid>.json
├── media/                      # localised media, bucketed by type
│   ├── thumb/                  # <vid>__<h6>.jpg
│   ├── avatar/                 # @mkbhd__<h6>.jpg
│   ├── banner/                 # @mkbhd__<h6>.jpg
│   ├── video/                  # <vid>__<fmt>.mp4 (only at --depth media)
│   └── audio/                  # <vid>__<fmt>.m4a (--depth audio, or -x)
├── _assets/
│   └── kura.css                # the one stylesheet the HTML views share
└── state.json                  # capture cursors, download resume offsets, visited state
```

Key points:

- **JSON is the source of truth.** Each video is `videos/<id>.json`, written the
  instant it arrives. The id is the 11-character string used verbatim, so the path
  is a pure function of the id and a re-capture overwrites the same file. A
  `.raw.json` sits beside it with the untouched upstream payload, so a parser
  improvement in ytb-cli can be replayed over an old archive.
- **Views are derived.** `html/`, `md/`, `index.html`, and `README.md` are all
  rebuilt from the JSON by the renderer. Delete them and `kura render <repo>`
  recreates them with no network.
- **Media is localised and deduped.** Files go under `media/<type>/`, named by the
  source key plus a short hash of the source URL. Two thumbnails never collide, and
  one avatar shared across many videos resolves to a single file. Stream files
  appear only at media or audio depth, and their name encodes the format selection.
- **Transcripts are stored twice.** Timed `.vtt` is the source; flat `.txt` makes
  the archive greppable for the spoken word.

## The manifest

`manifest.json` is the first file `kura info`, `kura add`, and `kura render` read.
Its record-bearing fields are sorted by id so a re-capture of the same content
writes a byte-identical manifest; the only wall-clock values live in the capture
entries.

| Field | Meaning |
|-------|---------|
| `service` | The source service, always `youtube` |
| `target` | What the repo archives: `kind`, `ref`, and the resolved `channel_id` for a channel |
| `depth` | The capture depth: `meta`, `media`, or `audio` |
| `videos` | Total records held |
| `media` | Counts of localised media: `thumbs`, `videos`, `audio` |
| `transcripts` | Number of transcripts captured |
| `comments_captured` | Whether comments were captured |
| `range` | The `oldest` and `newest` captured video timestamps |
| `captures` | One entry per run: `at` (the stamp), `added`, and `depth` |
| `gaps` | What an IP-gated or failed fetch could not capture: `video_id`, `what`, `reason` |
| `kura_version` | The kura version that wrote the repo |
| `schema` | The on-disk layout version, for future migration |

The `gaps` list is the archive being honest about its holes: a hidden comment
thread, an empty IP-gated transcript, a stream that failed the cipher. A gap
records exactly what is missing and why, rather than leaving the archive silently
incomplete.
