---
title: "Media and views"
description: "How thumbnails and streams are localised and deduped, choosing which views to render, the inert HTML watch page with its local video player, and re-rendering from stored JSON with no network."
weight: 60
---

A capture has two derived layers on top of the canonical JSON: the localised
media, and the rendered views. The media is controlled by the capture
[depth](/guides/depth-and-streams/); the views are controlled at capture time and
can both be rebuilt later from the JSON.

## How media is localised

Everything downloaded lands under `media/`, bucketed by type:

```
media/thumb/   <vid>__<h6>.jpg     the picked thumbnail
media/avatar/  @mkbhd__<h6>.jpg
media/banner/  @mkbhd__<h6>.jpg
media/video/   <vid>__<fmt>.mp4    only at --depth media
media/audio/   <vid>__<fmt>.m4a    --depth audio, or -x
```

Each file's name is the source key plus a short hash of its source URL, which
makes two things true: two thumbnails of one id never collide, and a channel
avatar referenced across a thousand videos resolves to a single file on disk,
downloaded once. The rendered pages rewrite their `src` attributes to these local
paths, so the archive opens with no network.

Thumbnails, avatars, and banners are fetched at every depth, through the same
client transport as the record reads, so they share its rate limiter, retry, and
cache. Streams are fetched only at media or audio depth; their filename encodes
the format selection, so two different selections of the same video coexist and a
re-run with the same selector finds its file already present.

The manifest is honest about what made it: a media or stream fetch that fails is
recorded, and the record still renders with a "media unavailable" or "stream not
archived" placeholder. A missing file degrades a page; it never aborts the
capture.

## The HTML watch page

The HTML view follows kage's philosophy: the output is inert. No `<script>`, no
`on*` handlers, no remote fonts, no analytics. `index.html` is the repo home (a
channel header and a grid of videos, or for a single video, the watch page
itself), and `html/<vid>.html` renders one video as a watch page: the title, the
channel row, the metrics, the full description with linkified timestamps, the
chapters, the inline transcript, and, when captured, the top comments.

At media depth the player region is a passive HTML5 `<video>` element over the
local file, with no autoplay JS. At meta depth it is the localised thumbnail with
a "stream not archived" caption. Either way it is a photograph that happens to
play, not a program.

## Choosing views

`--view` selects which rendered views to write. JSON and the sidecars are always
written regardless:

```bash
kura archive @mkbhd --view html       # HTML only (default)
kura archive @mkbhd --view md         # Markdown only
kura archive @mkbhd --view html,md    # both
```

The Markdown view gives you a `README.md` index plus per-video Markdown under
`md/`, each with a front-matter block, the description, the thumbnail as a
relative-path image, the chapter list, and the full transcript inline, so the
Markdown archive is a genuine greppable corpus.

## Re-rendering offline

Because the views are derived, you can rebuild them from the stored JSON at any
time with no network:

```bash
kura render $HOME/data/kura/youtube/@mkbhd
```

This is how you add a view to an archive, or replay a renderer improvement over an
old capture:

```bash
# Add Markdown to an archive that only had HTML
kura render $HOME/data/kura/youtube/@mkbhd --view md
```

`kura render` reads only the stored JSON and sidecars. It never touches the
network and never re-downloads media. Pass `--date` to fix the footer stamp for
reproducible output.

## Next

- The [CLI reference](/reference/cli/) lists every flag.
- [Repository layout](/reference/repository-layout/) maps what lands on disk.
