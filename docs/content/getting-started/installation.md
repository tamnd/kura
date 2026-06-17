---
title: "Installation"
description: "Install kura from Go, a release archive, a Linux package, or the container image, and add shell completion."
weight: 20
---

kura is a single binary. It needs no API key and no login. The only optional external tools are `ffmpeg` (for merging separate video and audio streams at media depth) and `yt-dlp` (for the cases the native engine declines), both discovered on your `PATH` and never bundled.

## Go

```bash
go install github.com/tamnd/kura/cmd/kura@latest
```

## Release archives and Linux packages

Every [release](https://github.com/tamnd/kura/releases) attaches `tar.gz`
archives (and a `.zip` for Windows) for Linux, macOS, Windows, and FreeBSD, plus
`.deb`, `.rpm`, and `.apk` packages and a `checksums.txt`. Download the one for
your platform, extract `kura`, and put it on your `PATH`.

```bash
# Debian/Ubuntu
sudo dpkg -i kura_*_linux_amd64.deb

# Fedora/RHEL
sudo rpm -i kura_*_linux_amd64.rpm
```

Homebrew and Scoop manifests publish alongside each release once their taps are
configured.

## Container

The image carries kura and nothing else. Mount a directory for the output and
point the archive at a target:

```bash
docker run --rm -v "$PWD/out:/out" ghcr.io/tamnd/kura archive dQw4w9WgXcQ
```

The archive lands in `./out/youtube/...` on your host. Set the output root with
`-o /out` if your mount differs, or with the `KURA_OUT` environment variable.

Note that the container image carries no `ffmpeg`. A media-depth capture inside
the container selects a muxed progressive format so a download still succeeds
without a merge.

## Shell completion

kura ships completion scripts for bash, zsh, fish, and PowerShell:

```bash
# zsh, for the current session
source <(kura completion zsh)

# bash, installed system-wide
kura completion bash | sudo tee /etc/bash_completion.d/kura
```

## Optional tools

kura is `CGO_ENABLED=0` and links no browser: it reads JSON surfaces and
downloads streams over HTTP, and the nsig solver runs in pure-Go goja. Two
external binaries are optional, never linked, and shared with the ytb-cli
toolchain so a setup that works for `ytb` works for `kura` unchanged:

```
YTB_FFMPEG_BIN    path to ffmpeg for the optional A/V merge (else PATH; else muxed-only)
YTB_YT_DLP_BIN    path to a yt-dlp binary for the optional --tool yt-dlp delegation
```

Without `ffmpeg`, a media-depth capture still works: kura selects a muxed
progressive format. Without `yt-dlp`, a stream the native engine cannot fetch is
recorded as a gap in the manifest and the video still archives at meta depth.

Next: [the quick start](/getting-started/quick-start/).
