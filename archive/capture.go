package archive

import (
	"context"
	"errors"
	"fmt"
	"regexp"
	"strconv"
	"strings"
	"time"

	"github.com/tamnd/kura/repo"
	"github.com/tamnd/kura/tool"
	"github.com/tamnd/ytb-cli/youtube"
)

// capturer holds the per-run capture state shared across the kind handlers.
type capturer struct {
	st     *repo.Store
	client *youtube.Client
	opts   Options
	log    Logf
	mf     *repo.Manifest
	seen   map[string]bool
	count  int         // videos processed (post-dedupe) this run, for the --max budget
	tool   *tool.YtDlp // resolved external downloader, or nil when --tool is unset

	// reachedSinceID records that paging stopped at the incremental boundary (a
	// known-archived id), which is a clean, complete stop rather than a partial one.
	reachedSinceID bool
	// complete records that the spine was captured exhaustively this run, written
	// into state.json so a later resume can page only newer uploads.
	complete bool
}

// captureChannel fetches and stores the channel record for a channel target so
// the header and avatar are available to every page. It returns nil for the
// other target kinds (a single video fetches its channel later; playlist and
// search archives have no single channel).
func (c *capturer) captureChannel(ctx context.Context, t Target) *youtube.Channel {
	if t.Kind != KindChannel {
		return nil
	}
	ch, err := c.client.FetchChannel(ctx, t.Ref)
	if err != nil {
		say(c.log, "channel %s: %v", t.Ref, err)
		return nil
	}
	if ch != nil {
		if err := c.st.WriteChannel(ch); err != nil {
			say(c.log, "write channel: %v", err)
		}
		say(c.log, "channel %s: %s, %d videos", ch.Handle, ch.Title, ch.VideoCount)
	}
	return ch
}

// captureSpine dispatches on the target kind, streaming the video spine into the
// store via captureVideo.
func (c *capturer) captureSpine(ctx context.Context, t Target, res *Result) error {
	switch t.Kind {
	case KindVideo:
		return c.captureVideoTarget(ctx, t.Ref, res)
	case KindChannel:
		return c.captureChannelTabs(ctx, t.Ref, res)
	case KindPlaylist:
		return c.capturePlaylist(ctx, t.Ref, res)
	case KindSearch:
		return c.captureSearch(ctx, t.Ref, res)
	default:
		return fmt.Errorf("unsupported target kind %q", t.Kind)
	}
}

// captureVideoTarget captures a single video and, for its byline, the owning
// channel. A fetch error here is fatal: the only target failed.
func (c *capturer) captureVideoTarget(ctx context.Context, id string, res *Result) error {
	if err := c.captureVideo(ctx, id, nil, res, true); err != nil {
		return err
	}
	if res.Channel == nil {
		if v, err := c.st.LoadVideo(youtube.ExtractVideoID(id)); err == nil && v != nil && v.ChannelID != "" {
			if ch, err := c.client.FetchChannel(ctx, v.ChannelID); err == nil && ch != nil {
				_ = c.st.WriteChannel(ch)
				res.Channel = ch
			}
		}
	}
	return nil
}

// captureChannelTabs streams the requested channel tabs (videos, and optionally
// shorts and live streams) plus community posts, capturing each video. A budget
// stop propagates; a single tab error is logged and the others still run.
func (c *capturer) captureChannelTabs(ctx context.Context, ref string, res *Result) error {
	tabs := []string{"videos"}
	if c.opts.Shorts {
		tabs = append(tabs, "shorts")
	}
	if c.opts.Streams {
		tabs = append(tabs, "streams")
	}
	for _, tab := range tabs {
		err := c.client.StreamChannelTab(ctx, ref, tab, youtube.PageOptions{Max: c.opts.Max}, func(v youtube.Video) error {
			return c.captureVideo(ctx, v.VideoID, &v, res, false)
		})
		if errors.Is(err, youtube.ErrStop) {
			return err
		}
		if err != nil {
			say(c.log, "tab %s: %v", tab, err)
		}
	}
	if c.opts.Community {
		c.captureCommunity(ctx, ref)
	}
	if c.opts.Playlists {
		c.capturePlaylistsTab(ctx, ref)
	}
	return nil
}

// capturePlaylist records the playlist and captures each of its videos in order.
func (c *capturer) capturePlaylist(ctx context.Context, ref string, res *Result) error {
	if pl, err := c.client.FetchPlaylist(ctx, ref); err == nil && pl != nil {
		_ = c.st.WriteJSON(repo.PlaylistJSON(pl.PlaylistID), pl)
	}
	return c.client.StreamPlaylistItems(ctx, ref, youtube.PageOptions{Max: c.opts.Max}, func(_ youtube.PlaylistVideo, v youtube.Video) error {
		return c.captureVideo(ctx, v.VideoID, &v, res, false)
	})
}

// capturePlaylistsTab records each playlist on a channel (widening flag).
func (c *capturer) capturePlaylistsTab(ctx context.Context, ref string) {
	err := c.client.StreamChannelPlaylists(ctx, ref, youtube.PageOptions{Max: c.opts.Max}, func(p youtube.Playlist) error {
		return c.st.WriteJSON(repo.PlaylistJSON(p.PlaylistID), p)
	})
	if err != nil && !errors.Is(err, youtube.ErrStop) {
		say(c.log, "playlists: %v", err)
	}
}

// captureSearch captures the video results of a search query.
func (c *capturer) captureSearch(ctx context.Context, query string, res *Result) error {
	filters := youtube.SearchFilters{Type: "video"}
	return c.client.Search(ctx, query, filters, youtube.PageOptions{Max: c.opts.Max}, func(item any) error {
		v, ok := item.(youtube.Video)
		if !ok || v.VideoID == "" {
			return nil
		}
		return c.captureVideo(ctx, v.VideoID, &v, res, false)
	})
}

// captureCommunity records community posts (widening flag).
func (c *capturer) captureCommunity(ctx context.Context, ref string) {
	err := c.client.StreamCommunity(ctx, ref, youtube.PageOptions{Max: c.opts.Max}, func(p youtube.CommunityPost) error {
		return c.st.WriteJSON(repo.CommunityJSON(p.PostID), p)
	})
	if err != nil && !errors.Is(err, youtube.ErrStop) {
		say(c.log, "community: %v", err)
	}
}

// captureVideo captures one video: its full record and sidecars. seed is the lite
// record from a spine stream (nil for a direct video target), used for the date
// filter before the heavier per-video fetch. It dedupes, honours the date bounds
// and the --max budget, writes the canonical record and the raw payload, and then
// fills any missing sidecars (transcript, comments, SponsorBlock). A record fetch
// error is fatal only for a single-video target; in a spine it is recorded as a
// gap and capture continues (KR4).
func (c *capturer) captureVideo(ctx context.Context, idOrURL string, seed *youtube.Video, res *Result, fatal bool) error {
	id := youtube.ExtractVideoID(idOrURL)
	if id == "" {
		id = idOrURL
	}
	if id == "" || c.seen[id] {
		return nil
	}
	// An incremental boundary: stop paging once a known-archived id is reached, so
	// `add --since-id` fetches only what is newer (the engine streams newest-first).
	if c.opts.SinceID != "" && id == c.opts.SinceID {
		c.reachedSinceID = true
		return youtube.ErrStop
	}
	c.seen[id] = true

	// Pre-filter on the seed's published time when the bounds are set and known.
	if seed != nil && !c.keepDate(seed.PublishedAt) {
		return nil
	}

	fresh := !c.st.HasVideo(id)
	if fresh || c.opts.Force {
		vr, err := c.client.FetchVideo(ctx, id, youtube.VideoOptions{Player: true, Next: true})
		if err != nil {
			if fatal {
				return err
			}
			say(c.log, "video %s: %v", id, err)
			c.mf.AddGap(id, "record", err.Error())
			return nil
		}
		v := vr.Video
		if !c.keepDate(v.PublishedAt) {
			return nil
		}
		if err := c.st.WriteVideo(&v, vr); err != nil {
			return err
		}
		if len(vr.Chapters) > 0 {
			_ = c.st.WriteJSON(repo.VideoChapters(id), vr.Chapters)
		}
		if fresh {
			res.Added++
		}
		if c.opts.Verbose {
			say(c.log, "  %s %s", id, oneLine(v.Title))
		}
	}

	c.captureSidecars(ctx, id)

	c.count++
	if c.opts.Max > 0 && c.count >= c.opts.Max {
		return youtube.ErrStop
	}
	return nil
}

// captureSidecars fills any sidecar that is requested and not already on disk, so
// a fresh capture writes them all and an `add --comments` backfills only the gap.
func (c *capturer) captureSidecars(ctx context.Context, id string) {
	if c.opts.wantTranscript() && !c.hasTranscript(id) {
		c.captureTranscript(ctx, id)
	}
	if c.opts.wantComments() && !c.st.Exists(repo.VideoComments(id)) {
		c.captureComments(ctx, id)
	}
	if c.opts.SponsorBlock && !c.st.Exists(repo.VideoSponsor(id)) {
		c.captureSponsor(ctx, id)
	}
}

// captureTranscript fetches and stores the transcript in each requested language
// (or the default track) as both a timed .vtt and a flat .txt. An IP-gated or
// missing transcript is recorded as a gap, not an error (KR4).
func (c *capturer) captureTranscript(ctx context.Context, id string) {
	langs := c.opts.Langs
	if len(langs) == 0 {
		langs = []string{""}
	}
	for _, lang := range langs {
		_, segs, err := c.client.Transcript(ctx, id, lang)
		if err != nil || len(segs) == 0 {
			// The engine got nothing (an IP-gated or empty transcript). A configured
			// tool is the rescue path before recording a gap.
			if c.toolTranscript(ctx, id, lang) {
				continue
			}
			if err != nil {
				c.mf.AddGap(id, "transcript", err.Error())
			}
			continue
		}
		_ = c.st.WriteText(repo.TranscriptVTT(id, lang), youtube.RenderSubtitles(segs, youtube.SubVTT))
		_ = c.st.WriteText(repo.TranscriptTXT(id, lang), youtube.RenderSubtitles(segs, youtube.SubText))
	}
}

// toolTranscript tries the configured external tool (yt-dlp) for a transcript the
// engine could not get, storing the .vtt and a flat .txt derived from it. It
// reports whether a transcript was written.
func (c *capturer) toolTranscript(ctx context.Context, id, lang string) bool {
	if c.tool == nil {
		return false
	}
	source := youtube.NormalizeVideoURL(id)
	vtt, err := c.tool.Subtitles(ctx, source, lang)
	if err != nil || len(vtt) == 0 {
		return false
	}
	if err := c.st.WriteText(repo.TranscriptVTT(id, lang), string(vtt)); err != nil {
		return false
	}
	_ = c.st.WriteText(repo.TranscriptTXT(id, lang), vttToText(string(vtt)))
	return true
}

// vttToText flattens a WebVTT body into greppable plain text: it drops the header,
// cue numbers, timestamp and setting lines, strips inline tags like <c> and the
// <00:00:01.000> karaoke marks, and collapses the consecutive duplicate lines that
// auto-captions roll out, so the .txt reads as prose.
func vttToText(vtt string) string {
	var b strings.Builder
	var prev string
	for line := range strings.SplitSeq(vtt, "\n") {
		line = strings.TrimSpace(line)
		switch {
		case line == "", strings.Contains(line, "-->"):
			continue
		case strings.HasPrefix(line, "WEBVTT"), strings.HasPrefix(line, "Kind:"), strings.HasPrefix(line, "Language:"), strings.HasPrefix(line, "NOTE"):
			continue
		}
		if _, err := strconv.Atoi(line); err == nil {
			continue // a bare cue number
		}
		line = strings.TrimSpace(vttTag.ReplaceAllString(line, ""))
		if line == "" || line == prev {
			continue
		}
		b.WriteString(line)
		b.WriteByte('\n')
		prev = line
	}
	return b.String()
}

// vttTag matches the inline markup WebVTT cues carry: voice/class spans like
// <c.colorE5E5E5> and the per-word timestamps <00:00:01.000>.
var vttTag = regexp.MustCompile(`<[^>]*>`)

// captureComments streams the comment tree into a sidecar. Restricted-Mode
// hiding is recorded as a gap (exit 4 at the CLI), not an error.
func (c *capturer) captureComments(ctx context.Context, id string) {
	opt := youtube.CommentOptions{
		Max:     c.opts.MaxComments,
		Replies: c.opts.CommentReplies,
		Sort:    c.opts.CommentSort,
	}
	var comments []youtube.Comment
	err := c.client.StreamComments(ctx, id, opt, func(cm youtube.Comment) error {
		comments = append(comments, cm)
		return nil
	})
	if err != nil && !errors.Is(err, youtube.ErrStop) {
		if errors.Is(err, youtube.ErrCommentsRestricted) {
			c.mf.AddGap(id, "comments", "hidden by Restricted Mode")
		} else {
			c.mf.AddGap(id, "comments", err.Error())
		}
		if len(comments) == 0 {
			return
		}
	}
	_ = c.st.WriteJSON(repo.VideoComments(id), comments)
}

// captureSponsor fetches the crowd-sourced SponsorBlock segments into a sidecar.
func (c *capturer) captureSponsor(ctx context.Context, id string) {
	segs, err := c.client.SponsorSegments(ctx, id, nil)
	if err != nil {
		return // SponsorBlock is best-effort; a miss is not even a gap
	}
	if len(segs) == 0 {
		return
	}
	_ = c.st.WriteJSON(repo.VideoSponsor(id), segs)
}

// keepDate applies the --since/--until bounds to a published time. A zero time
// (unknown) is kept so a record with no date is never silently dropped.
func (c *capturer) keepDate(t time.Time) bool {
	if t.IsZero() {
		return true
	}
	if !c.opts.Since.IsZero() && t.Before(c.opts.Since) {
		return false
	}
	if !c.opts.Until.IsZero() && !t.Before(c.opts.Until) {
		return false
	}
	return true
}

// hasTranscript reports whether any transcript for the video is on disk, probing
// the run's languages plus the auto track.
func (c *capturer) hasTranscript(id string) bool {
	langs := append([]string{""}, c.opts.Langs...)
	for _, lang := range langs {
		if c.st.Exists(repo.TranscriptVTT(id, lang)) {
			return true
		}
	}
	return false
}

// countTranscripts counts how many videos have a transcript on disk.
func (c *capturer) countTranscripts(all []*youtube.Video) int {
	n := 0
	for _, v := range all {
		if c.hasTranscript(v.VideoID) {
			n++
		}
	}
	return n
}
