// Package cli is the kura command tree: a cobra hierarchy wrapped with
// charmbracelet/fang for polished help, version, and error rendering (spec §12).
// The commands are thin — they parse flags, build a ytb-cli client, call the
// archive/render packages, and map the outcome onto stable exit codes (spec §12).
// No scraping or rendering logic lives here; kura reads YouTube only through the
// ytb-cli engine (KR1) and renders only through render/ (KR3).
package cli

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"strings"
	"time"

	"github.com/charmbracelet/fang"
	"github.com/spf13/cobra"
	"github.com/tamnd/ytb-cli/youtube"
)

// Exit codes, matching ytb-cli's grammar (spec §12). A script can branch on the
// outcome of a capture.
const (
	CodeOK        = 0   // success
	CodeUsage     = 1   // usage or config error
	CodePartial   = 2   // some records/sidecars/streams failed but the repo was written
	CodeNoResults = 3   // the target resolved but yielded nothing
	CodeGated     = 4   // a surface is IP-gated (comments/transcript hidden)
	CodeBlocked   = 5   // blocked by anti-bot or rate-limit
	CodeNotFound  = 6   // target not found, or ffmpeg required-but-missing for a merge
	CodeNeedsTool = 7   // an external tool (yt-dlp) was requested but absent
	CodeInterrupt = 130 // interrupted (state flushed for --resume)
)

// exitError carries an explicit process exit code out of a command, so a capture
// that wrote a repository but hit a gated surface or a partial failure reports the
// right code without looking like a usage error. fang renders the wrapped message.
type exitError struct {
	code int
	err  error
}

func (e *exitError) Error() string {
	if e.err == nil {
		return ""
	}
	return e.err.Error()
}

func (e *exitError) Unwrap() error { return e.err }

// withCode wraps err with an explicit exit code. A nil err with a non-zero code
// is a coded outcome with no rendered message.
func withCode(code int, err error) error {
	if code == CodeOK {
		return err
	}
	return &exitError{code: code, err: err}
}

// Execute builds the command tree, runs it through fang, and returns a process
// exit code. cmd/kura/main.go passes a signal-aware context so Ctrl-C cancels the
// in-flight capture and exits 130.
//
// The version string is set on the root command itself (see newRootCmd) so
// `kura --version` reports the commit and build date alongside the version;
// WithoutVersion stops fang from overwriting it with a version-only line.
// renderError replaces fang's default handler, which title-cases the first word
// of the message (mangling a leading file path) and prints an empty ERROR box
// for a coded outcome that carries no message.
func Execute(ctx context.Context) int {
	root := newRootCmd()
	err := fang.Execute(ctx, root,
		fang.WithoutVersion(),
		fang.WithErrorHandler(renderError),
	)
	return codeFor(ctx, err)
}

// renderError prints err in fang's ERROR box, but faithfully: it keeps the
// message verbatim (no title-casing, no forced trailing period) so a leading
// file path survives, and prints nothing at all when the message is empty, which
// is how a coded outcome with no human message (a gated capture that still wrote
// a repository) reaches here.
func renderError(w io.Writer, styles fang.Styles, err error) {
	msg := err.Error()
	if msg == "" {
		return
	}
	_, _ = fmt.Fprintln(w, styles.ErrorHeader.String())
	_, _ = fmt.Fprintln(w, styles.ErrorText.UnsetTransform().Render(msg))
	_, _ = fmt.Fprintln(w)
}

// codeFor maps an error (already rendered by fang) onto an exit code.
func codeFor(ctx context.Context, err error) int {
	if err == nil {
		return CodeOK
	}
	if ctx.Err() != nil || errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
		return CodeInterrupt
	}
	if ee, ok := errors.AsType[*exitError](err); ok {
		return ee.code
	}
	if errors.Is(err, youtube.ErrFFmpegMissing) {
		return CodeNotFound
	}
	if errors.Is(err, youtube.ErrCommentsRestricted) {
		return CodeGated
	}
	return classifyMessage(err)
}

// classifyMessage maps a plain engine error onto an exit code by its surface
// markers. ytb-cli returns unwrapped fmt errors, so the HTTP status and the
// "unavailable/private" language are read from the message.
func classifyMessage(err error) int {
	msg := strings.ToLower(err.Error())
	switch {
	case strings.Contains(msg, "429"), strings.Contains(msg, "rate"), strings.Contains(msg, "captcha"), strings.Contains(msg, "blocked"):
		return CodeBlocked
	// Tool-availability markers are checked before the generic "not found" bucket
	// so that "yt-dlp not found" reports a missing tool rather than a missing target.
	case strings.Contains(msg, "yt-dlp"):
		return CodeNeedsTool
	case strings.Contains(msg, "ffmpeg"):
		return CodeNotFound
	case strings.Contains(msg, "404"), strings.Contains(msg, "not found"),
		strings.Contains(msg, "unavailable"), strings.Contains(msg, "private"),
		strings.Contains(msg, "removed"), strings.Contains(msg, "terminated"):
		return CodeNotFound
	default:
		return CodeUsage
	}
}

func newRootCmd() *cobra.Command {
	root := &cobra.Command{
		Use:   "kura",
		Short: "Build offline, browsable archives of YouTube content",
		Long: "kura (蔵, storehouse) captures YouTube videos, channels, playlists and\n" +
			"searches into self-contained archives: canonical JSON, localised\n" +
			"thumbnails and streams, inline transcripts, and inert HTML and Markdown\n" +
			"views that open with the network unplugged.\n\n" +
			"It reads YouTube through the free InnerTube surface of the ytb-cli engine,\n" +
			"with no API key and no account. Metadata, thumbnails and transcripts come\n" +
			"for free; --depth media downloads the playable stream via the pure-Go\n" +
			"engine (ffmpeg only for the final A/V merge).",
		Version:       fmt.Sprintf("%s (commit %s, built %s)", Version, Commit, Date),
		SilenceUsage:  true,
		SilenceErrors: true,
		// Resolve config-file and environment defaults for every fetching command
		// before it runs, in the precedence flags > env > config file > built-in
		// default. Lives on the root so each subcommand inherits it.
		PersistentPreRunE: resolveDefaults,
	}

	// Access and politeness, shared by every fetching command (delegated to the
	// engine config). Output and capture flags live on the individual commands.
	pf := root.PersistentFlags()
	pf.Duration("rate", 0, "minimum delay between requests (default: engine default)")
	pf.Int("retries", -1, "retry attempts on a transient failure (default: engine default)")
	pf.Duration("timeout", 0, "per-request timeout (default: engine default)")
	pf.Int("workers", 0, "concurrent request workers (default: engine default)")
	pf.String("hl", "", "interface language code (e.g. en)")
	pf.String("gl", "", "content country code (e.g. US)")
	pf.Bool("no-cache", false, "bypass the shared on-disk cache")
	pf.BoolP("verbose", "v", false, "log each record as it is captured")

	root.AddCommand(
		newArchiveCmd(false),
		newArchiveCmd(true),
		newRenderCmd(),
		newServeCmd(),
		newInfoCmd(),
	)
	return root
}

// clientFromFlags builds a ytb-cli client from the zero-setup defaults and the
// shared persistent flags. There are no credentials anywhere: kura reads the free
// InnerTube surface only (spec §13).
func clientFromFlags(cmd *cobra.Command) *youtube.Client {
	cfg := youtube.DefaultConfig()

	f := cmd.Flags()
	if v, _ := f.GetDuration("rate"); v > 0 {
		cfg.Delay = v
	}
	if v, _ := f.GetInt("retries"); v >= 0 {
		cfg.Retries = v
	}
	if v, _ := f.GetDuration("timeout"); v > 0 {
		cfg.Timeout = v
	}
	if v, _ := f.GetInt("workers"); v > 0 {
		cfg.Workers = v
	}
	if v, _ := f.GetString("hl"); v != "" {
		cfg.HL = v
	}
	if v, _ := f.GetString("gl"); v != "" {
		cfg.GL = v
	}
	return youtube.NewClient(cfg)
}

// stderrLog returns a progress sink that writes to stderr when verbose is set,
// else a nil (silent) sink.
func stderrLog(cmd *cobra.Command) func(string, ...any) {
	if v, _ := cmd.Flags().GetBool("verbose"); !v {
		return nil
	}
	return func(format string, args ...any) {
		fmt.Fprintf(os.Stderr, format+"\n", args...)
	}
}

// parseDate parses an optional RFC3339 capture stamp.
func parseDate(s string) (time.Time, error) {
	if s == "" {
		return time.Time{}, nil
	}
	return time.Parse(time.RFC3339, s)
}

// parseLooseTime parses a timeline bound as either RFC3339 or a bare calendar
// date (2006-01-02, interpreted as UTC midnight).
func parseLooseTime(s string) (time.Time, error) {
	s = strings.TrimSpace(s)
	if s == "" {
		return time.Time{}, nil
	}
	if t, err := time.Parse(time.RFC3339, s); err == nil {
		return t, nil
	}
	return time.Parse("2006-01-02", s)
}
