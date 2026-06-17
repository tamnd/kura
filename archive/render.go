package archive

import (
	"fmt"
	"os"
	"sort"
	"strconv"
	"strings"
	"time"

	"github.com/tamnd/kura/media"
	"github.com/tamnd/kura/render"
	"github.com/tamnd/kura/render/html"
	"github.com/tamnd/kura/render/md"
	"github.com/tamnd/kura/repo"
	"github.com/tamnd/ytb-cli/youtube"
)

// RenderOptions configures a no-network re-render of an existing repository.
type RenderOptions struct {
	Views   []string  // which shapes to write: "html", "md"
	Date    time.Time // footer stamp (zero = now)
	Version string    // kura build version
}

// RenderResult is the summary of a re-render.
type RenderResult struct {
	Root   string
	Videos int
}

// Render re-builds the HTML and Markdown views of an existing repository from its
// stored JSON, with no network access (spec §5). It is how a renderer improvement
// replays over an old archive and how a Markdown view is added to an HTML-only
// repo. The records, sidecars, media index, and target identity all come off disk.
func Render(root string, opts RenderOptions) (*RenderResult, error) {
	mf, ok, err := repo.LoadManifest(root)
	if err != nil {
		return nil, err
	}
	if !ok || mf == nil {
		return nil, fmt.Errorf("%s is not a kura repository (no manifest.json)", root)
	}
	st, err := repo.Open(root)
	if err != nil {
		return nil, err
	}
	all, err := st.LoadVideos()
	if err != nil {
		return nil, err
	}
	channel, _, err := st.LoadChannel()
	if err != nil {
		return nil, err
	}

	views := opts.Views
	if len(views) == 0 {
		views = []string{"html"}
	}
	ro := Options{
		Views:   views,
		Date:    opts.Date,
		Version: opts.Version,
		Depth:   media.Depth(mf.Depth),
		Langs:   nil,
	}
	t := targetFromManifest(mf)
	if err := renderAll(st, all, channel, mf.MediaIndex, t, ro); err != nil {
		return nil, err
	}
	return &RenderResult{Root: root, Videos: len(all)}, nil
}

// targetFromManifest reconstructs the capture target identity from the stored
// manifest, enough for the render headings and nav (no fetch).
func targetFromManifest(mf *repo.Manifest) Target {
	tr := mf.Target
	t := Target{Kind: Kind(tr.Kind), Ref: tr.Ref}
	switch t.Kind {
	case KindChannel:
		t.Display = channelDisplay(tr.Ref)
	case KindSearch:
		t.Display = "Search: " + tr.Ref
	case KindPlaylist:
		t.Display = "Playlist " + tr.Ref
	case KindVideo:
		t.Display = "Video " + tr.Ref
	default:
		t.Display = tr.Ref
	}
	return t
}

// renderAll writes the requested human views (HTML site, Markdown archive) over
// the merged record set. It loads each video's sidecars into a render.Bundle,
// orders the index newest-first, and writes the shared stylesheet, the home page,
// and a page per video. The view model is pure (render/), so this stage only
// reads the store and writes the derived files; nothing here touches the network
// or the clock beyond the capture stamp baked into the footer.
func renderAll(st *repo.Store, all []*youtube.Video, channel *youtube.Channel, assets []repo.Asset, t Target, opts Options) error {
	if !opts.wantHTML() && !opts.wantMD() {
		return nil
	}

	bundles := make([]render.Bundle, 0, len(all))
	for _, v := range all {
		bundles = append(bundles, loadBundle(st, v, opts))
	}
	// The index reads newest-first; the per-video files keep their id paths, so
	// only the listing order depends on this sort (KR5: stable, content-derived).
	sort.SliceStable(bundles, func(i, j int) bool {
		return bundles[i].Video.PublishedAt.After(bundles[j].Video.PublishedAt)
	})

	nav := navTitle(t, channel)
	footer := footerLine(opts)
	heading, subheading := indexLabels(t, channel)

	if opts.wantHTML() {
		if err := st.WriteText(repo.CSSFile, string(html.CSS())); err != nil {
			return err
		}
		hr := html.New(all, assets, channel, footer, nav)
		index, err := hr.Index(bundles, heading, subheading)
		if err != nil {
			return err
		}
		if err := st.WriteText(repo.IndexHTML, index); err != nil {
			return err
		}
		for _, b := range bundles {
			page, err := hr.VideoPage(b)
			if err != nil {
				return err
			}
			if page == "" {
				continue
			}
			if err := st.WriteText(repo.VideoHTML(b.Video.VideoID), page); err != nil {
				return err
			}
		}
	}

	if opts.wantMD() {
		mr := md.New(all, assets, channel, footer, nav)
		if err := st.WriteText(repo.ReadmeMD, mr.Index(bundles, heading, subheading)); err != nil {
			return err
		}
		for _, b := range bundles {
			doc := mr.Video(b)
			if doc == "" {
				continue
			}
			if err := st.WriteText(repo.VideoMD(b.Video.VideoID), doc); err != nil {
				return err
			}
		}
	}
	return nil
}

// loadBundle gathers a video and its sidecars from the store into the render
// bundle. A missing sidecar is simply absent, never an error: the views degrade
// gracefully to what was captured (KR4).
func loadBundle(st *repo.Store, v *youtube.Video, opts Options) render.Bundle {
	b := render.Bundle{Video: v}
	if chs, err := st.LoadChapters(v.VideoID); err == nil {
		b.Chapters = chs
	}
	if cs, err := st.LoadComments(v.VideoID); err == nil {
		b.Comments = cs
	}
	if sp, err := st.LoadSponsor(v.VideoID); err == nil {
		b.Sponsor = sp
	}
	b.Transcript, b.TransLang = loadTranscript(st, v.VideoID, opts)
	return b
}

// loadTranscript reads the first available transcript for a video, probing the
// run's requested languages and then the default auto track, and parses its VTT
// back into timed segments for the inline transcript.
func loadTranscript(st *repo.Store, id string, opts Options) ([]youtube.TranscriptSegment, string) {
	langs := append([]string{}, opts.Langs...)
	langs = append(langs, "")
	for _, lang := range langs {
		rel := repo.TranscriptVTT(id, lang)
		if !st.Exists(rel) {
			continue
		}
		b, err := os.ReadFile(st.Abs(rel))
		if err != nil {
			continue
		}
		segs := parseVTT(string(b))
		if len(segs) == 0 {
			continue
		}
		return segs, strings.TrimSpace(lang)
	}
	return nil, ""
}

// parseVTT reads a WebVTT body into transcript segments, keeping each cue's start
// time and text. It is the inverse of the engine's VTT renderer for the fields
// the views use (start + text); cue settings and styling are ignored.
func parseVTT(body string) []youtube.TranscriptSegment {
	var out []youtube.TranscriptSegment
	lines := strings.Split(strings.ReplaceAll(body, "\r\n", "\n"), "\n")
	for i := 0; i < len(lines); i++ {
		line := strings.TrimSpace(lines[i])
		before, _, ok := strings.Cut(line, "-->")
		if !ok {
			continue
		}
		start := parseVTTTime(strings.TrimSpace(before))
		if start < 0 {
			continue
		}
		var text []string
		for j := i + 1; j < len(lines); j++ {
			t := strings.TrimSpace(lines[j])
			if t == "" {
				i = j
				break
			}
			text = append(text, t)
			i = j
		}
		joined := strings.TrimSpace(strings.Join(text, " "))
		if joined == "" {
			continue
		}
		out = append(out, youtube.TranscriptSegment{StartSeconds: start, Text: joined})
	}
	return out
}

// parseVTTTime parses an HH:MM:SS.mmm or MM:SS.mmm cue time into seconds, or -1
// when the token is not a timestamp.
func parseVTTTime(s string) float64 {
	if s == "" {
		return -1
	}
	parts := strings.Split(s, ":")
	if len(parts) < 2 || len(parts) > 3 {
		return -1
	}
	var secs float64
	for _, p := range parts {
		f, err := strconv.ParseFloat(strings.Replace(p, ",", ".", 1), 64)
		if err != nil {
			return -1
		}
		secs = secs*60 + f
	}
	return secs
}

// navTitle is the repository's display name in the nav and headings: the channel
// title or handle when a channel was captured, else the target's display label.
func navTitle(t Target, channel *youtube.Channel) string {
	if channel != nil {
		if channel.Title != "" {
			return channel.Title
		}
		if channel.Handle != "" {
			return channel.Handle
		}
	}
	return t.Display
}

// indexLabels are the heading and subheading for a non-channel home page (a
// search or a playlist capture). A channel archive uses its own header, so it
// returns empty labels.
func indexLabels(t Target, channel *youtube.Channel) (string, string) {
	if channel != nil {
		return "", ""
	}
	switch t.Kind {
	case KindSearch:
		return "Search results", t.Ref
	case KindPlaylist:
		return "Playlist", t.Ref
	case KindVideo:
		return "", ""
	default:
		return t.Display, ""
	}
}

// footerLine is the page footer carrying the capture stamp (the only wall-clock
// value in a rendered page, KR5).
func footerLine(opts Options) string {
	return "Archived with kura on " + opts.stamp().Format("2006-01-02") + "."
}
