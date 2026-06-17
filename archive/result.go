package archive

import "github.com/tamnd/ytb-cli/youtube"

// Result is the summary of one capture, returned to the CLI for the closing
// report and the exit code.
type Result struct {
	Root   string
	Target Target
	DryRun bool

	Channel *youtube.Channel

	Videos      int  // total video records in the repo after this run
	Added       int  // records new to the repo this run
	Transcripts int  // transcripts on disk
	Comments    bool // comment sidecars were captured

	MediaOK   int // images localised or reused
	MediaFail int // images that failed to localise

	StreamOK   int // streams localised or reused
	StreamFail int // streams that failed
	StreamOnly int // streams present at YouTube but not archivable as a file

	Oldest string // RFC3339 published time of the oldest captured video
	Newest string // RFC3339 published time of the newest captured video

	Gaps  int // recorded holes (gated surfaces, failed fetches)
	Gated int // holes from an IP-gated surface (comments/transcript hidden)

	notes []string
}

// Note records a one-line advisory for the CLI to print after the run.
func (r *Result) Note(s string) { r.notes = append(r.notes, s) }

// Notes returns the recorded advisories.
func (r *Result) Notes() []string { return r.notes }
