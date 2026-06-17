// Package render turns stored records into the two human views: the kage-shape
// HTML site (render/html) and the yomi-shape Markdown archive (render/md). Both
// derive from the same view model built here (KR3), so the two views always
// agree on what a video says: the linkified description, the localised thumbnail
// and stream, the chapter list, the inline transcript, and the top comments. The
// view model is pure, records and a path context in and a view struct out, so
// the renderers carry golden tests with no network and no clock (spec §14).
package render

import (
	"fmt"
	"html/template"
	"path"
	"sort"
	"strings"
	"time"

	"github.com/tamnd/kura/repo"
	"github.com/tamnd/ytb-cli/youtube"
)

// Bundle is one video and every sidecar the renderers draw from. The archive
// loads it from the store; the view model never reads disk itself.
type Bundle struct {
	Video      *youtube.Video
	Chapters   []youtube.Chapter
	Comments   []youtube.Comment
	Transcript []youtube.TranscriptSegment
	TransLang  string
	Sponsor    []youtube.SponsorSegment
}

// Context resolves references the way a self-contained archive needs: it knows
// which video ids are in the archive (so an in-archive link is relative and an
// outside link stays absolute), which media source URLs are localised, and which
// videos have a downloaded stream on disk (so the player points at the local
// file). FromPage is the repo-relative path of the page being rendered, set by
// the renderer before each build, so every reference is rewritten relative to it
// (KR2, spec §6.5).
type Context struct {
	InArchive  map[string]bool   // video id -> record present
	MediaPath  map[string]string // media source URL -> repo-relative local path
	StreamPath map[string]string // video id -> repo-relative local video stream
	AudioPath  map[string]string // video id -> repo-relative local audio stream
	FromPage   string
}

// NewContext builds a render context from a record set and the localised media
// index. Stream and audio assets carry a "video:<id>" / "audio:<id>" key so the
// context can map a video to its playable file; image assets map by source URL.
func NewContext(videos []*youtube.Video, assets []repo.Asset) *Context {
	c := &Context{
		InArchive:  make(map[string]bool, len(videos)),
		MediaPath:  map[string]string{},
		StreamPath: map[string]string{},
		AudioPath:  map[string]string{},
	}
	for _, v := range videos {
		if v != nil {
			c.InArchive[v.VideoID] = true
		}
	}
	for _, a := range assets {
		if a.Status != repo.StatusLocal || a.Path == "" {
			continue
		}
		switch {
		case strings.HasPrefix(a.Key, "video:"):
			c.StreamPath[strings.TrimPrefix(a.Key, "video:")] = a.Path
		case strings.HasPrefix(a.Key, "audio:"):
			c.AudioPath[strings.TrimPrefix(a.Key, "audio:")] = a.Path
		default:
			if a.Source != "" {
				c.MediaPath[a.Source] = a.Path
			}
		}
	}
	return c
}

// CommentView is one rendered comment (or reply) with its resolved avatar.
type CommentView struct {
	Author      string
	AvatarSrc   string
	AvatarLocal bool
	HTMLBody    template.HTML
	TextBody    string
	Likes       int64
	Replies     int
	Stamp       string
	IsOwner     bool
	IsReply     bool
}

// ChapterView is one chapter marker with a formatted offset and a jump target.
type ChapterView struct {
	Title  string
	Offset string // mm:ss or h:mm:ss
	Start  int    // seconds
	Jump   string // local "#t=NN" when the stream is on disk, else a youtube &t= link
}

// SegmentView is one transcript line with a formatted offset.
type SegmentView struct {
	Offset string
	Text   string
}

// VideoView is the shared, presentation-agnostic view of one video. The HTML
// renderer reads HTMLBody and the template fields; the Markdown renderer reads
// the text fields. Both see the same chapters, transcript, and comments.
type VideoView struct {
	ID           string
	Title        string
	URL          string // canonical youtube watch URL (the source)
	Permalink    string // page-relative link to this video's own local page
	ChannelID    string
	ChannelName  string
	ChannelRel   string // page-relative link to the channel index when in archive, else youtube
	AvatarSrc    string
	AvatarLocal  bool
	PublishedAt  time.Time
	Stamp        string
	Duration     string
	Views        int64
	Likes        int64
	CommentCount int64
	HTMLBody     template.HTML // description, linkified and escaped
	TextBody     string        // raw description
	Hashtags     []string
	IsShort      bool
	IsLive       bool

	// Player: ThumbSrc is the localised (or remote) poster; StreamSrc is the
	// local video file when one was downloaded (media depth), else empty.
	ThumbSrc   string
	ThumbLocal bool
	StreamSrc  string // local <video> source, "" at meta depth
	AudioSrc   string // local audio source when audio-only depth

	Chapters   []ChapterView
	Transcript []SegmentView
	TransLang  string
	Comments   []CommentView
	Sponsor    []youtube.SponsorSegment
}

// HasStream reports whether the video has a playable local file.
func (v VideoView) HasStream() bool { return v.StreamSrc != "" || v.AudioSrc != "" }

// Build constructs a VideoView for one bundle under the context.
func (c *Context) Build(b Bundle) VideoView {
	if b.Video == nil {
		return VideoView{}
	}
	v := b.Video
	stream := c.StreamPath[v.VideoID]
	audio := c.AudioPath[v.VideoID]
	view := VideoView{
		ID:           v.VideoID,
		Title:        v.Title,
		URL:          v.URL,
		ChannelID:    v.ChannelID,
		ChannelName:  v.ChannelName,
		PublishedAt:  v.PublishedAt,
		Stamp:        stampOf(v),
		Duration:     v.DurationText,
		Views:        v.ViewCount,
		Likes:        v.LikeCount,
		CommentCount: v.CommentCount,
		TextBody:     v.Description,
		Hashtags:     v.Hashtags,
		IsShort:      v.IsShort,
		IsLive:       v.IsLive,
		TransLang:    b.TransLang,
		Sponsor:      b.Sponsor,
	}
	if view.URL == "" {
		view.URL = youtube.NormalizeVideoURL(v.VideoID)
	}
	// The description links timestamps into whatever player this page has.
	jump := jumpTarget{vid: v.VideoID, url: view.URL, local: stream != "" || audio != ""}
	view.HTMLBody = linkifyHTML(v.Description, jump)
	view.ThumbSrc, view.ThumbLocal = c.resolveURL(v.ThumbnailURL)
	if stream != "" {
		view.StreamSrc = c.rel(stream)
	}
	if audio != "" {
		view.AudioSrc = c.rel(audio)
	}
	if c.InArchive[v.VideoID] {
		if rel := c.linkToVideo(v.VideoID); rel != "" && rel != path.Base(c.FromPage) {
			view.Permalink = rel
		}
	}
	view.ChannelRel = c.linkToChannel(v.ChannelID)
	view.Chapters = c.chapterViews(b.Chapters, jump)
	view.Transcript = segmentViews(b.Transcript)
	view.Comments = c.commentViews(b.Comments)
	return view
}

// SetAvatar fills the channel avatar from a captured channel record. It is a
// separate step because the avatar source lives on the Channel, not the Video.
func (c *Context) SetAvatar(v *VideoView, ch *youtube.Channel) {
	if ch == nil {
		return
	}
	if src, ok := c.MediaSrc(ch.AvatarURL); ok {
		v.AvatarSrc, v.AvatarLocal = src, true
	} else if ch.AvatarURL != "" {
		v.AvatarSrc = ch.AvatarURL
	}
}

func (c *Context) chapterViews(chs []youtube.Chapter, j jumpTarget) []ChapterView {
	if len(chs) == 0 {
		return nil
	}
	ordered := append([]youtube.Chapter(nil), chs...)
	sort.SliceStable(ordered, func(i, k int) bool { return ordered[i].StartSeconds < ordered[k].StartSeconds })
	out := make([]ChapterView, 0, len(ordered))
	for _, ch := range ordered {
		out = append(out, ChapterView{
			Title:  ch.Title,
			Offset: clock(ch.StartSeconds),
			Start:  ch.StartSeconds,
			Jump:   j.at(ch.StartSeconds),
		})
	}
	return out
}

func segmentViews(segs []youtube.TranscriptSegment) []SegmentView {
	if len(segs) == 0 {
		return nil
	}
	out := make([]SegmentView, 0, len(segs))
	for _, s := range segs {
		text := strings.TrimSpace(s.Text)
		if text == "" {
			continue
		}
		out = append(out, SegmentView{Offset: clock(int(s.StartSeconds)), Text: text})
	}
	return out
}

func (c *Context) commentViews(comments []youtube.Comment) []CommentView {
	if len(comments) == 0 {
		return nil
	}
	out := make([]CommentView, 0, len(comments))
	for _, cm := range comments {
		cv := CommentView{
			Author:   cm.AuthorDisplayName,
			HTMLBody: linkifyHTML(cm.TextDisplay, jumpTarget{}),
			TextBody: cm.TextDisplay,
			Likes:    cm.LikeCount,
			Replies:  cm.ReplyCount,
			Stamp:    cm.PublishedText,
			IsOwner:  cm.IsOwnerComment,
			IsReply:  cm.ParentID != "",
		}
		if src, ok := c.MediaSrc(cm.AuthorProfileImage); ok {
			cv.AvatarSrc, cv.AvatarLocal = src, true
		} else {
			cv.AvatarSrc = cm.AuthorProfileImage
		}
		out = append(out, cv)
	}
	return out
}

// resolveURL maps a source URL to a page-relative local path when on disk, else
// returns the URL unchanged (an outside reference stays absolute).
func (c *Context) resolveURL(srcURL string) (string, bool) {
	if srcURL == "" {
		return "", false
	}
	if p, ok := c.MediaPath[srcURL]; ok && p != "" {
		return c.rel(p), true
	}
	return srcURL, false
}

// MediaSrc resolves a source URL to a page-relative local path, reporting false
// when the asset is not localised.
func (c *Context) MediaSrc(srcURL string) (string, bool) {
	if p, ok := c.MediaPath[srcURL]; ok && p != "" {
		return c.rel(p), true
	}
	return "", false
}

func (c *Context) rel(repoPath string) string {
	if c.FromPage == "" {
		return repoPath
	}
	return repo.Rel(c.FromPage, repoPath)
}

// linkToVideo returns a page-relative link to another video's HTML page when it
// is in the archive, else the absolute youtube watch URL.
func (c *Context) linkToVideo(id string) string {
	if id == "" {
		return ""
	}
	if c.InArchive[id] {
		return c.rel(repo.VideoHTML(id))
	}
	return youtube.NormalizeVideoURL(id)
}

// linkToChannel returns a link to the channel index. A kura repo's home is the
// channel page, so an in-archive channel resolves to index.html; an outside
// channel links to youtube.
func (c *Context) linkToChannel(channelID string) string {
	if channelID == "" {
		return ""
	}
	return youtube.NormalizeChannelURL(channelID)
}

// jumpTarget formats a timestamp link: into the local player when the stream is
// on disk, else into the youtube watch URL with a &t= parameter.
type jumpTarget struct {
	vid   string
	url   string
	local bool
}

func (j jumpTarget) at(sec int) string {
	if j.local {
		return fmt.Sprintf("#t=%d", sec)
	}
	if j.url == "" {
		return ""
	}
	sep := "?"
	if strings.Contains(j.url, "?") {
		sep = "&"
	}
	return fmt.Sprintf("%s%st=%d", j.url, sep, sec)
}

// clock formats a second offset as mm:ss or h:mm:ss.
func clock(sec int) string {
	if sec < 0 {
		sec = 0
	}
	h := sec / 3600
	m := (sec % 3600) / 60
	s := sec % 60
	if h > 0 {
		return fmt.Sprintf("%d:%02d:%02d", h, m, s)
	}
	return fmt.Sprintf("%d:%02d", m, s)
}

func stampOf(v *youtube.Video) string {
	if !v.PublishedAt.IsZero() {
		return v.PublishedAt.UTC().Format("2006-01-02")
	}
	return v.PublishedText
}

// FormatCount renders a count compactly (1.2K, 3.4M), the way YouTube shows it.
func FormatCount(n int64) string {
	switch {
	case n >= 1_000_000_000:
		return trimZero(float64(n)/1_000_000_000) + "B"
	case n >= 1_000_000:
		return trimZero(float64(n)/1_000_000) + "M"
	case n >= 1_000:
		return trimZero(float64(n)/1_000) + "K"
	default:
		return fmt.Sprintf("%d", n)
	}
}

func trimZero(f float64) string {
	s := fmt.Sprintf("%.1f", f)
	return strings.TrimSuffix(s, ".0")
}
