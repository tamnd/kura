---
title: "CLI reference"
description: "Every kura command, flag, and exit code."
weight: 10
---

```
kura [command] [flags]
```

The commands: `archive` captures a target into a repository, `add` (alias
`update`) re-captures an existing one incrementally, `render` rebuilds the views
from stored JSON, `serve` previews a repository over HTTP, `info` summarises one,
and `completion` writes a shell completion script. `kura --version` prints the
build version, commit, and date. Run `kura <command> --help` for the canonical,
up-to-date list.

## Global flags

These persistent flags apply to every fetching command. They configure the shared
ytb-cli engine: politeness, locale, and caching. kura holds no API key and needs
no login.

| Flag | Default | Meaning |
|------|---------|---------|
| `--rate` | engine default | Minimum delay between requests |
| `--retries` | engine default | Retry attempts on a transient failure |
| `--timeout` | engine default | Per-request timeout |
| `--no-cache` | `false` | Bypass the on-disk response cache |
| `--hl` | engine default | Interface language for the InnerTube reads |
| `--gl` | engine default | Country for the InnerTube reads |
| `-v, --verbose` | `false` | Log the surface and endpoint per record as it is captured |

## kura archive

```
kura archive <target>... [flags]
```

Captures one or more targets into a repository at `<out>/youtube/<root>`. A target
is a video id or watch URL, a channel `@handle`, `UC...` id, or vanity path, or a
`PL...`/`UU...` playlist id; the selector flags below switch to a search, album,
or transcripts-only capture. Records are written as JSON as they arrive, then
media is localised and the views rendered.

### Target selectors

| Flag | Default | Meaning |
|------|---------|---------|
| `--search` | | Capture a search query instead of a channel |
| `--album` | | Capture a music album by id |
| `--transcripts-only` | `false` | Capture each video's transcript text only |

### Capture depth

| Flag | Default | Meaning |
|------|---------|---------|
| `--depth` | `meta` | Whether and how to localise streams: `meta`, `media`, or `audio` |

### Channel widening

| Flag | Default | Meaning |
|------|---------|---------|
| `--shorts` | `false` | Include the channel's Shorts |
| `--streams` | `false` | Include the channel's past live streams |
| `--playlists` | `false` | Capture the channel's playlists and their order |
| `--community` | `false` | Capture community posts |

### Sidecars

| Flag | Default | Meaning |
|------|---------|---------|
| `--comments` | `false` | Capture the comment thread per video |
| `--max-comments` | | Cap the number of comments captured per video |
| `--sort` | `top` | Comment sort: `top` or `new` |
| `--sponsorblock` | `false` | Capture SponsorBlock segments per video |
| `--lang` | default track | Transcript language(s) to store (comma-separated) |

### Record shaping

| Flag | Default | Meaning |
|------|---------|---------|
| `--since` | | Only videos at or after this time (RFC3339 or `2006-01-02`) |
| `--until` | | Only videos before this time (RFC3339 or `2006-01-02`) |
| `--since-id` | | Only videos newer than this id |
| `--max` | `0` | Record budget (0 = all the surface gives; defaults to 1000 for a channel or search; a single video or playlist is unbounded) |

### Streams

Delegated to the native download engine. Used at `--depth media` or `--depth
audio`.

| Flag | Default | Meaning |
|------|---------|---------|
| `-f, --format` | `bv*+ba/b` (with ffmpeg, else `b`) | `yt-dlp`-grammar format selection |
| `-x, --audio-only` | `false` | Select the audio stream only (implies audio depth) |
| `--quality` | | Cap the video height |
| `--ffmpeg-bin` | `$YTB_FFMPEG_BIN` or PATH | ffmpeg path for the A/V merge |
| `--tool` | | Delegate the hard cases to an external tool (e.g. `yt-dlp`) |
| `--concurrent` | engine default | Ranged download workers |

### Output and rendering

| Flag | Default | Meaning |
|------|---------|---------|
| `--view` | `html` | Views to render: `html`, `md`, or `html,md` (JSON is always written) |
| `-o, --out` | `$HOME/data/kura` | Output root; the repo lands at `<out>/youtube/<root>` |
| `--date` | capture time | Fix the capture stamp (RFC3339) for reproducible output |
| `--resume` | `true` | Continue from held state and resume a half-downloaded stream |
| `--force` | `false` | Ignore held state and recapture from scratch |
| `--dry-run` | `false` | Print the capture plan without fetching |

The output root, like the other configurable defaults, also reads the config file
and the `KURA_OUT` environment variable when `-o/--out` is not given. See
[configuration and environment](#configuration-and-environment).

## kura add

```
kura add <target>... [flags]
```

Alias: `kura update`. The same capture machinery as `kura archive`, but it
defaults to the incremental path: fetch only what is newer than the newest record
already on disk, then re-render only the affected pages. It takes every flag `kura
archive` does. `kura add --depth media` over a meta repo upgrades a catalog to a
playable vault, fetching only the streams.

## kura render

```
kura render <repo> [flags]
```

Re-renders the HTML and Markdown views from the stored JSON with no network. This
adds a view to an archive, or replays a renderer change over an old one.

| Flag | Default | Meaning |
|------|---------|---------|
| `--view` | `html` | Views to render: `html`, `md`, or `html,md` |
| `--date` | | Fix the footer stamp (RFC3339) for reproducible output |

## kura info

```
kura info <repo>
```

Prints a manifest summary: the service and target, the capture depth, video,
transcript, and media counts, the date range, the capture history, the recorded
gaps, and the on-disk size. Takes no flags.

## kura serve

```
kura serve <repo> [flags]
```

Runs a local static file server over a repository so links, media, and the
`<video>` range requests resolve as they would on a host. The archive is already
self-contained, so this is a convenience over opening `index.html` directly.

| Flag | Default | Meaning |
|------|---------|---------|
| `--addr` | `127.0.0.1:8080` | Address to listen on |

## Configuration and environment

kura reads no API key. A flag default can come from the config file or the
environment. Resolution order, later wins: built-in defaults, then the config
file, then the environment, then the flag on the command line.

The config file is a plain list of `key = value` lines; blank lines and lines
starting with `#` or `;` are ignored, and a value may be quoted. kura reads
`$KURA_CONFIG` if set, else `$XDG_CONFIG_HOME/kura/config`, else
`~/.config/kura/config`. A missing file is not an error.

```
# ~/.config/kura/config
out   = /vault
depth = media
view  = html,md
rate  = 750ms
```

Each configurable default has a config-file key, a matching `KURA_*` variable,
and a flag:

| Key | Variable | Flag | Meaning |
|-----|----------|------|---------|
| `out` | `KURA_OUT` | `--out` | Default output root (else `$HOME/data/kura`) |
| `depth` | `KURA_DEPTH` | `--depth` | Default depth (`meta`, `media`, or `audio`) |
| `view` | `KURA_VIEW` | `--view` | Default views (`html`, `md`, or `html,md`) |
| `format` | `KURA_FORMAT` | `--format` | Default format selector |
| `quality` | `KURA_QUALITY` | `--quality` | Cap the video height (e.g. `1080`) |
| `tool` | `KURA_TOOL` | `--tool` | External downloader (e.g. yt-dlp) |
| `ffmpeg` | `KURA_FFMPEG` | `--ffmpeg-bin` | Path to ffmpeg for the A/V merge |
| `rate` | `KURA_RATE` | `--rate` | Minimum delay between requests |
| `retries` | `KURA_RETRIES` | `--retries` | Retry attempts on a transient failure |
| `timeout` | `KURA_TIMEOUT` | `--timeout` | Per-request timeout |
| `workers` | `KURA_WORKERS` | `--workers` | Concurrent request workers |
| `hl` | `KURA_HL` | `--hl` | Interface language code |
| `gl` | `KURA_GL` | `--gl` | Content country code |
| `no_cache` | `KURA_NO_CACHE` | `--no-cache` | Bypass the shared on-disk cache |

The optional external tools are shared with the ytb-cli toolchain:

| Variable | Meaning |
|----------|---------|
| `YTB_YT_DLP_BIN` | Path to a yt-dlp binary for `--tool yt-dlp` |
| `YTB_FFMPEG_BIN` | Path to ffmpeg for the A/V merge (else PATH; else muxed-only) |
| `NO_COLOR` | Honoured by the styles |

## Exit codes

A script can branch on the outcome of any command:

| Code | Name | Meaning |
|------|------|---------|
| `0` | ok | Captured successfully |
| `1` | usage | Bad flag, malformed target, or other usage error |
| `2` | partial | Some records, sidecars, or streams failed but the repo was written |
| `3` | no-results | The target resolved but yielded nothing |
| `4` | ip-gated | A surface is IP-gated (comments or transcript hidden); names the gap and the yt-dlp fallback |
| `5` | blocked | Blocked by anti-bot or rate-limited |
| `6` | not-found | The target does not exist (deleted, private, region-locked), or ffmpeg was required but missing for a merge |
| `7` | tool-missing | An external tool (`yt-dlp`) was requested but absent |
| `130` | interrupted | Cancelled with Ctrl-C; state is flushed for `--resume` |
