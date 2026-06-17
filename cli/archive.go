package cli

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/spf13/cobra"
	"github.com/tamnd/kura/archive"
	"github.com/tamnd/kura/media"
)

// newArchiveCmd builds either `kura archive` or, when add is true, `kura add`.
// They share the same capture machinery; add is the friendlier name for a re-run
// that fetches only what is new and re-renders (spec §11).
func newArchiveCmd(add bool) *cobra.Command {
	use := "archive <target>..."
	short := "Capture a target into a new or existing repository"
	long := "Capture one or more targets into a self-contained repository under\n" +
		"<out>/youtube/<root>/. A target is a video id or watch URL, an @handle or\n" +
		"channel URL, a playlist id, or --search/--album. Records always persist as\n" +
		"JSON; --depth media adds the playable stream."
	var aliases []string
	if add {
		use = "add <target>..."
		short = "Fetch only what is new for an existing target and re-render"
		long = "Incrementally capture into an existing repository: fetch the records and\n" +
			"sidecars that are not on disk yet, localise any new media, and re-render\n" +
			"the views. `add --depth media` upgrades a metadata catalog to a playable\n" +
			"vault in place."
		aliases = []string{"update"}
	}

	cmd := &cobra.Command{
		Use:     use,
		Aliases: aliases,
		Short:   short,
		Long:    long,
		Example: archiveExamples,
		Args:    cobra.ArbitraryArgs,
		RunE:    runArchive,
	}

	f := cmd.Flags()

	// Target selectors.
	f.String("search", "", "capture a search query instead of a channel/video")
	f.String("album", "", "capture a playlist by id (alias: --playlist)")
	f.String("playlist", "", "capture a playlist by id")

	// Depth and capture mode.
	f.String("depth", "meta", "how much to localise: meta|media|audio")
	f.Bool("transcripts-only", false, "capture transcripts as a corpus; no comments, no streams")

	// Channel widening.
	f.Bool("shorts", false, "include the channel's Shorts tab")
	f.Bool("streams", false, "include the channel's live/streams tab")
	f.Bool("playlists", false, "also record the channel's playlists")
	f.Bool("community", false, "also record the channel's community posts")

	// Sidecars.
	f.Bool("comments", false, "capture the comment tree as a sidecar")
	f.Int("max-comments", 0, "comment budget per video (0 = engine default)")
	f.String("sort", "top", "comment order: top|new")
	f.Bool("replies", false, "include comment replies")
	f.Bool("sponsorblock", false, "capture crowd-sourced SponsorBlock segments")

	// Transcripts.
	f.StringSlice("lang", nil, "transcript language code(s) to store (default: the default track)")

	// Record shaping.
	f.Int("max", 0, "record budget (0 = all the surface gives; default 1000 for a channel/search)")
	f.String("since", "", "only videos at or after this time (RFC3339 or 2006-01-02)")
	f.String("until", "", "only videos before this time (RFC3339 or 2006-01-02)")
	f.String("since-id", "", "stop paging once this already-archived video id is reached")

	// Streams (delegated to the native engine).
	f.StringP("format", "f", "", "yt-dlp-grammar format selector (default: depth default)")
	f.BoolP("audio-only", "x", false, "download audio only (same as --depth audio)")
	f.Int("quality", 0, "cap the video height (e.g. 1080)")
	f.String("ffmpeg-bin", "", "path to ffmpeg for the A/V merge (else PATH)")
	f.String("tool", "", "external downloader for the hard cases (e.g. yt-dlp)")
	f.Int("concurrent", 0, "per-stream range concurrency (default: engine default)")

	// Output and rendering.
	f.String("view", defaultView(), "views to render: html|md|html,md (JSON is always written)")
	f.StringP("out", "o", defaultOut(), "output root; the repo lands at <out>/youtube/<root>")
	f.String("date", "", "fix the capture stamp (RFC3339) for reproducible output")
	f.Bool("resume", true, "continue from an existing repository")
	f.Bool("force", false, "re-capture records already on disk")
	f.Bool("dry-run", false, "print the capture plan without fetching")

	return cmd
}

const archiveExamples = `  kura archive dQw4w9WgXcQ                       # one video: metadata, thumbnail, transcript
  kura archive dQw4w9WgXcQ --depth media         # ...and download the playable stream
  kura archive @mkbhd                            # channel catalog: every upload, no streams
  kura archive @mkbhd --depth media -f bv*+ba/b  # full vault: every upload, merged mp4
  kura archive @lexfridman --transcripts-only    # a greppable spoken-word corpus
  kura archive PLxxxx --depth audio -x           # a playlist as an offline audio archive
  kura archive --search "lofi mix" --max 200
  kura archive @mkbhd --comments --sponsorblock  # add comment + segment sidecars
  kura add @mkbhd                                # fetch only new uploads, re-render
  kura add @mkbhd --depth media                  # upgrade the catalog to a playable vault
  kura render ~/data/kura/youtube/@mkbhd --view md`

func runArchive(cmd *cobra.Command, args []string) error {
	f := cmd.Flags()

	sel := archive.Selector{}
	sel.Search, _ = f.GetString("search")
	if a, _ := f.GetString("album"); a != "" {
		sel.Playlist = a
	}
	if p, _ := f.GetString("playlist"); p != "" {
		sel.Playlist = p
	}

	opts, err := optionsFromFlags(cmd)
	if err != nil {
		return err
	}

	// Targets: the positional args, or a single flag-specified target.
	rawTargets := args
	if len(rawTargets) == 0 {
		rawTargets = []string{""}
	}

	client := clientFromFlags(cmd)
	log := stderrLog(cmd)
	ctx := cmd.Context()

	var firstErr error
	worst := CodeOK
	for _, raw := range rawTargets {
		t, err := archive.ParseTarget(raw, sel)
		if err != nil {
			return err // a malformed target is a usage error (exit 1)
		}

		runOpts := opts
		applyDefaultBudget(cmd, &runOpts, t)

		res, err := archive.Run(ctx, client, t, runOpts, log)
		printResult(cmd, res)
		if err != nil && firstErr == nil {
			firstErr = err
		}
		worst = worseCode(worst, codeForRun(ctx, res, err))

		// Only one flag-specified target makes sense per run.
		if sel.Search != "" || sel.Playlist != "" {
			break
		}
	}
	return withCode(worst, firstErr)
}

// optionsFromFlags builds the capture option set, shared by every target in one
// run (the per-target record budget is applied later).
func optionsFromFlags(cmd *cobra.Command) (archive.Options, error) {
	f := cmd.Flags()
	var o archive.Options
	o.Version = Version

	o.Out, _ = f.GetString("out")
	o.TranscriptsOnly, _ = f.GetBool("transcripts-only")
	o.Shorts, _ = f.GetBool("shorts")
	o.Streams, _ = f.GetBool("streams")
	o.Playlists, _ = f.GetBool("playlists")
	o.Community, _ = f.GetBool("community")
	o.Comments, _ = f.GetBool("comments")
	o.MaxComments, _ = f.GetInt("max-comments")
	o.CommentReplies, _ = f.GetBool("replies")
	o.SponsorBlock, _ = f.GetBool("sponsorblock")
	o.Langs, _ = f.GetStringSlice("lang")
	o.Max, _ = f.GetInt("max")
	o.Format, _ = f.GetString("format")
	o.Quality, _ = f.GetInt("quality")
	o.FFmpeg, _ = f.GetString("ffmpeg-bin")
	o.Tool, _ = f.GetString("tool")
	o.Workers, _ = f.GetInt("concurrent")
	o.Resume, _ = f.GetBool("resume")
	o.Force, _ = f.GetBool("force")
	o.DryRun, _ = f.GetBool("dry-run")
	o.Verbose, _ = f.GetBool("verbose")

	switch s, _ := f.GetString("sort"); s {
	case "top", "new":
		o.CommentSort = s
	case "":
		o.CommentSort = "top"
	default:
		return o, fmt.Errorf("invalid --sort %q (want top|new)", s)
	}

	depth, ok := media.ParseDepth(mustString(f, "depth"))
	if !ok {
		return o, fmt.Errorf("invalid --depth %q (want meta|media|audio)", mustString(f, "depth"))
	}
	if x, _ := f.GetBool("audio-only"); x {
		depth = media.DepthAudio
	}
	o.Depth = depth

	since, _ := f.GetString("since")
	if t, err := parseLooseTime(since); err != nil {
		return o, fmt.Errorf("parse --since: %w", err)
	} else {
		o.Since = t
	}
	until, _ := f.GetString("until")
	if t, err := parseLooseTime(until); err != nil {
		return o, fmt.Errorf("parse --until: %w", err)
	} else {
		o.Until = t
	}
	o.SinceID, _ = f.GetString("since-id")

	if t, err := parseDate(mustString(f, "date")); err != nil {
		return o, fmt.Errorf("parse --date: %w", err)
	} else {
		o.Date = t
	}

	views, err := parseViews(mustString(f, "view"))
	if err != nil {
		return o, err
	}
	o.Views = views

	return o, nil
}

// applyDefaultBudget gives an unbounded channel or search capture a default
// record budget when the user did not pass --max, so a bare `kura archive @handle`
// does not try to pull a whole history by accident (spec §12.1). A single video or
// playlist stays unbounded.
func applyDefaultBudget(cmd *cobra.Command, o *archive.Options, t archive.Target) {
	if cmd.Flags().Changed("max") {
		return
	}
	if t.Kind == archive.KindChannel || t.Kind == archive.KindSearch {
		o.Max = 1000
	}
}

func parseViews(s string) ([]string, error) {
	var out []string
	for p := range strings.SplitSeq(s, ",") {
		p = strings.TrimSpace(strings.ToLower(p))
		switch p {
		case "":
			continue
		case "html", "md":
			out = append(out, p)
		default:
			return nil, fmt.Errorf("invalid --view %q (want html, md, or html,md)", p)
		}
	}
	if len(out) == 0 {
		return nil, fmt.Errorf("--view selects no view")
	}
	return out, nil
}

// codeForRun derives the exit code for one capture from its result and error.
func codeForRun(ctx context.Context, res *archive.Result, err error) int {
	if err != nil {
		// A hard failure: classify the engine error, but if a repository was still
		// written it is a partial outcome rather than a total miss.
		c := codeFor(ctx, err)
		if c == CodeUsage && res != nil && res.Videos > 0 {
			return CodePartial
		}
		return c
	}
	if res == nil || res.DryRun {
		return CodeOK
	}
	switch {
	case res.Videos == 0:
		return CodeNoResults
	case res.Gated > 0:
		return CodeGated
	case res.StreamFail > 0 || res.MediaFail > 0:
		return CodePartial
	default:
		return CodeOK
	}
}

// worseCode returns the more severe of two exit codes, with OK the least severe.
func worseCode(a, b int) int {
	if b == CodeOK {
		return a
	}
	if a == CodeOK {
		return b
	}
	if b > a {
		return b
	}
	return a
}

// printResult writes a short human summary of one capture to stdout. The summary
// is assembled in a buffer and flushed once so the rendering path has a single
// write to check.
func printResult(cmd *cobra.Command, res *archive.Result) {
	if res == nil {
		return
	}
	var b strings.Builder
	if res.DryRun {
		fmt.Fprintf(&b, "dry run · %s → %s\n", res.Target.Display, res.Root)
		_, _ = fmt.Fprint(cmd.OutOrStdout(), b.String())
		return
	}
	fmt.Fprintf(&b, "%s\n", res.Target.Display)
	fmt.Fprintf(&b, "  repo:        %s\n", res.Root)
	fmt.Fprintf(&b, "  videos:      %d total (+%d new)\n", res.Videos, res.Added)
	fmt.Fprintf(&b, "  transcripts: %d\n", res.Transcripts)
	if res.Oldest != "" {
		fmt.Fprintf(&b, "  range:       %s … %s\n", short(res.Oldest), short(res.Newest))
	}
	fmt.Fprintf(&b, "  media:       %d local", res.MediaOK)
	if res.MediaFail > 0 {
		fmt.Fprintf(&b, ", %d unavailable", res.MediaFail)
	}
	fmt.Fprintln(&b)
	if res.StreamOK > 0 || res.StreamFail > 0 || res.StreamOnly > 0 {
		fmt.Fprintf(&b, "  streams:     %d local", res.StreamOK)
		if res.StreamFail > 0 {
			fmt.Fprintf(&b, ", %d failed", res.StreamFail)
		}
		if res.StreamOnly > 0 {
			fmt.Fprintf(&b, ", %d stream-only", res.StreamOnly)
		}
		fmt.Fprintln(&b)
	}
	if res.Gaps > 0 {
		fmt.Fprintf(&b, "  gaps:        %d\n", res.Gaps)
	}
	for _, n := range res.Notes() {
		fmt.Fprintf(&b, "  note:        %s\n", n)
	}
	_, _ = fmt.Fprint(cmd.OutOrStdout(), b.String())
}

// short trims an RFC3339 stamp to its date for the summary line.
func short(s string) string {
	if len(s) >= 10 {
		return s[:10]
	}
	return s
}

// defaultOut is the output root: $KURA_OUT, else $HOME/data/kura.
func defaultOut() string {
	if v := os.Getenv("KURA_OUT"); v != "" {
		return v
	}
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		return "kura-out"
	}
	return filepath.Join(home, "data", "kura")
}

// defaultView is the default --view: $KURA_VIEW, else html.
func defaultView() string {
	if v := os.Getenv("KURA_VIEW"); v != "" {
		return v
	}
	return "html"
}

func mustString(f interface{ GetString(string) (string, error) }, name string) string {
	v, _ := f.GetString(name)
	return v
}
