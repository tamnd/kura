// Package archive is the capture pipeline that turns a target into a kura
// repository: it resolves what to fetch on the free InnerTube surface, streams
// the records through the ytb-cli engine into the store, localises the media,
// renders the HTML and Markdown views, and writes the manifest (spec §5, §7). It
// owns no scraping of its own — every byte from YouTube comes through the engine
// (KR1) — and no presentation — every page comes through render/ (KR3). What it
// adds is the artifact: a self-contained, deterministic, resumable archive.
package archive

import (
	"slices"
	"time"

	"github.com/tamnd/kura/media"
)

// Options is the resolved configuration for one capture, built from the CLI
// flags. The zero value is not valid; the CLI fills it.
type Options struct {
	// Output.
	Out   string   // output root; the repo lands at <Out>/youtube/<root>
	Views []string // which rendered shapes to write: "html", "md"

	// Depth and streams (delegated to the native engine).
	Depth   media.Depth // meta | media | audio
	Format  string      // yt-dlp-grammar -f selector ("" = depth default)
	Quality int         // height cap, when > 0
	FFmpeg  string      // explicit ffmpeg path ("" = auto-locate)
	Tool    string      // optional external downloader (yt-dlp) for the hard cases
	Workers int         // per-stream range concurrency (--concurrent)

	// Channel widening (spec §12.1).
	Shorts    bool
	Streams   bool
	Playlists bool
	Community bool

	// Sidecars.
	Comments       bool
	MaxComments    int
	CommentSort    string // top | new
	CommentReplies bool
	SponsorBlock   bool

	// Transcripts.
	Langs           []string // language codes to store ("" => the default track)
	TranscriptsOnly bool     // capture transcripts as a corpus; no comments, no streams

	// Record shaping.
	Max     int       // total record budget; 0 = all the surface gives
	Since   time.Time // only videos published at or after this time
	Until   time.Time // only videos published before this time
	SinceID string    // stop paging once this already-archived video id is reached

	// Run control.
	Date    time.Time // capture stamp written into the manifest (KR5); zero means now
	Resume  bool      // continue from an existing repo (default on)
	Force   bool      // re-capture records already on disk
	DryRun  bool      // plan only; fetch and write nothing
	Verbose bool      // log each record as it is captured
	Version string    // kura build version, recorded in the manifest
}

// wantHTML reports whether the HTML view should be rendered.
func (o Options) wantHTML() bool { return o.hasView("html") }

// wantMD reports whether the Markdown view should be rendered.
func (o Options) wantMD() bool { return o.hasView("md") }

func (o Options) hasView(v string) bool { return slices.Contains(o.Views, v) }

// wantTranscript reports whether transcripts should be captured. They are on by
// default (meta depth keeps them) and are the whole point of --transcripts-only.
func (o Options) wantTranscript() bool { return true }

// wantComments reports whether comment sidecars should be captured.
func (o Options) wantComments() bool { return o.Comments && !o.TranscriptsOnly }

// wantStreams reports whether stream bytes should be localised.
func (o Options) wantStreams() bool { return o.Depth.WantsStream() && !o.TranscriptsOnly }

// stamp returns the capture timestamp to record, defaulting to now when unset.
func (o Options) stamp() time.Time {
	if o.Date.IsZero() {
		return time.Now().UTC()
	}
	return o.Date.UTC()
}

// streamOptions builds the media-layer options for one stream download.
func (o Options) streamOptions() media.StreamOptions {
	return media.StreamOptions{
		Depth:   o.Depth,
		Format:  o.Format,
		FFmpeg:  o.FFmpeg,
		Workers: o.Workers,
	}
}
