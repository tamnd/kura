---
title: "Configuration"
description: "Set kura's defaults once with a config file and KURA_* environment variables, understand the defaults to config to env to flag precedence, tune the shared response cache, and point kura at an external yt-dlp or ffmpeg."
weight: 70
---

kura runs with zero configuration: every flag has a sensible default and there is no API key to set.
But once you settle on an output root, a default depth, or a format selector you reach for on every run, you should not have to retype it.
This guide is about saying it once.

## Where a default comes from

Every configurable default is resolved from four places, and the later one always wins:

1. The flag's built-in default.
2. The config file.
3. The matching `KURA_*` environment variable.
4. The flag on the command line.

So `--out /vault` on the command line always wins.
With no `--out`, kura takes `$KURA_OUT`.
With neither, it takes the `out` key from your config file.
With none of those, it falls back to the built-in `$HOME/data/kura`.

The rule is the same for every key in the table below, which means you can keep a stable baseline in the config file, override it per-shell with an environment variable, and still reach past both with a one-off flag.

## The config file

The config file is a plain list of `key = value` lines.
Blank lines and lines starting with `#` or `;` are ignored, and a value may be wrapped in single or double quotes if it has trailing spaces you want to keep.
A missing file is not an error.

kura reads the first of these that exists:

1. `$KURA_CONFIG`, if set, as an explicit path.
2. `$XDG_CONFIG_HOME/kura/config`.
3. `~/.config/kura/config`.

A realistic config that turns kura into a media-vault builder for one library:

```
# ~/.config/kura/config

out    = /vault              # every archive lands under here
depth  = media               # download streams by default, not just metadata
view   = html,md             # render both views
format = bv*+ba/b            # best video + best audio, merged
rate   = 750ms               # be polite on long channel walks
```

With that file in place, `kura archive @mkbhd` writes a playable vault under `/vault/youtube/@mkbhd/` with both an HTML and a Markdown view, and you never typed a flag.

## The keys

Each configurable default has a config-file key, a matching `KURA_*` environment variable, and a flag.
They are interchangeable names for the same setting.

| Key | Variable | Flag | Meaning |
|-----|----------|------|---------|
| `out` | `KURA_OUT` | `--out` | Output root (else `$HOME/data/kura`) |
| `depth` | `KURA_DEPTH` | `--depth` | Default depth: `meta`, `media`, or `audio` |
| `view` | `KURA_VIEW` | `--view` | Default views: `html`, `md`, or `html,md` |
| `format` | `KURA_FORMAT` | `--format` | Default `yt-dlp`-grammar format selector |
| `quality` | `KURA_QUALITY` | `--quality` | Cap the video height, e.g. `1080` |
| `tool` | `KURA_TOOL` | `--tool` | External downloader, e.g. `yt-dlp` |
| `ffmpeg` | `KURA_FFMPEG` | `--ffmpeg-bin` | Path to ffmpeg for the A/V merge |
| `rate` | `KURA_RATE` | `--rate` | Minimum delay between requests |
| `retries` | `KURA_RETRIES` | `--retries` | Retry attempts on a transient failure |
| `timeout` | `KURA_TIMEOUT` | `--timeout` | Per-request timeout |
| `workers` | `KURA_WORKERS` | `--workers` | Concurrent request workers |
| `hl` | `KURA_HL` | `--hl` | Interface language code |
| `gl` | `KURA_GL` | `--gl` | Content country code |
| `no_cache` | `KURA_NO_CACHE` | `--no-cache` | Bypass the shared response cache |

A flag that a given subcommand does not define is simply skipped, so a `format` key in your config is honoured by `archive` and `add` and ignored by `info`.

## Tuning the response cache

kura keeps a shared on-disk cache of the InnerTube reads it makes, so a re-render, a re-run, or dev iteration is fast and works offline.
The cache lives in your user cache directory, not inside any archive, so deleting it only costs a re-fetch.
It is resolved from the first of:

1. `$KURA_CACHE`, as an explicit path.
2. `$XDG_CACHE_HOME/kura`.
3. `~/.cache/kura`.

A cached read is served only while it is fresh.
The freshness window defaults to one hour; widen or narrow it with `KURA_CACHE_TTL`, which takes a Go duration like `30m` or `6h`:

```bash
export KURA_CACHE_TTL=6h            # trust a cached listing for six hours
kura archive @mkbhd --no-cache      # or ignore the cache entirely for the freshest counts
```

Only the metadata, channel, and listing reads are cached.
Stream bytes and thumbnails always pass through, because their de-duplication is the local archive itself.
See [incremental and resumable captures](/guides/incremental-and-resumable/) for how the cache and the resume cursor work together.

## Pointing at external tools

Two optional binaries are never bundled and never linked, so the shipped kura stays a single static binary.
When you want them, name them.

ffmpeg performs the audio-plus-video merge at `--depth media`.
kura finds it via `--ffmpeg-bin`, then `$YTB_FFMPEG_BIN`, then your `PATH`; without it, kura selects a muxed progressive format so a download still succeeds.

`yt-dlp` is an opt-in rescue path for the stream and transcript cases the native engine declines.
Turn it on with `--tool yt-dlp` (or the `tool` config key), and point kura at the binary with `$YTB_YT_DLP_BIN` if it is not on your `PATH`:

```bash
export YTB_YT_DLP_BIN=/opt/bin/yt-dlp
kura archive dQw4w9WgXcQ --depth media --tool yt-dlp
```

kura reuses ytb-cli's `YTB_*` tool variables unchanged, so a toolchain that already works for `ytb` works for `kura` with no extra setup.

| Variable | Meaning |
|----------|---------|
| `KURA_CACHE` | Cache root (else `$XDG_CACHE_HOME/kura`, else `~/.cache/kura`) |
| `KURA_CACHE_TTL` | Response freshness window as a Go duration (default `1h`) |
| `YTB_YT_DLP_BIN` | Path to a yt-dlp binary for `--tool yt-dlp` |
| `YTB_FFMPEG_BIN` | Path to ffmpeg for the A/V merge (else `PATH`; else muxed-only) |
| `NO_COLOR` | Honoured by the terminal styles |

## Next

- The [CLI reference](/reference/cli/) lists every flag and exit code.
- [Depth and streams](/guides/depth-and-streams/) covers format selection and the merge in depth.
- [Incremental and resumable captures](/guides/incremental-and-resumable/) covers the cache and the resume cursor.
