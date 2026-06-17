// Package media localises an archive's media: the images (video thumbnails,
// channel avatar and banner, comment author avatars) and, at media or audio
// depth, the streams. Images are fetched directly through the engine's HTTP
// client so they ride the same transport as the records; streams go through the
// engine's pure-Go download path (manifest -> format select -> ranged fetch ->
// optional ffmpeg merge). name.go and plan.go are pure (no network); images.go
// and streams.go touch the wire. Every localised file lands at a deterministic
// path (KR5) and is recorded in the manifest's media index with a status, so the
// archive is honest about what it could and could not pull (KR4).
package media

import (
	"strconv"
	"strings"

	"github.com/tamnd/kura/repo"
	"github.com/tamnd/ytb-cli/youtube"
)

// Depth controls how much of a video kura localises (spec §7).
type Depth string

const (
	// DepthMeta captures records, thumbnails, transcripts, chapters and (when
	// asked) comments, but no stream bytes. It is the default.
	DepthMeta Depth = "meta"
	// DepthMedia adds the video+audio stream, merged to a playable file.
	DepthMedia Depth = "media"
	// DepthAudio adds the audio stream only.
	DepthAudio Depth = "audio"
)

// ParseDepth normalises a depth flag, defaulting to meta.
func ParseDepth(s string) (Depth, bool) {
	switch Depth(strings.ToLower(strings.TrimSpace(s))) {
	case DepthMeta, "":
		return DepthMeta, true
	case DepthMedia:
		return DepthMedia, true
	case DepthAudio:
		return DepthAudio, true
	default:
		return DepthMeta, false
	}
}

// WantsStream reports whether a depth localises stream bytes.
func (d Depth) WantsStream() bool { return d == DepthMedia || d == DepthAudio }

// Logf is the optional progress sink shared by the image and stream passes.
type Logf func(format string, args ...any)

// Item is one image reference to fetch and where it lands. Streams are not
// planned as Items because choosing a rendition needs the live manifest; they
// are handled imperatively in streams.go.
type Item struct {
	Key    string // "thumb:<id>", "avatar:<handle>", "banner:<handle>", "cavatar:<hash>"
	Type   string // thumb | avatar | banner
	Source string // the URL to fetch (also the render lookup key)
	Path   string // repo-relative destination
}

// ThumbItem builds the thumbnail download item for a video, or false when the
// record carries no thumbnail URL (render resolves the poster by this exact
// source URL, so the asset must keep it).
func ThumbItem(v *youtube.Video) (Item, bool) {
	if v == nil || v.ThumbnailURL == "" {
		return Item{}, false
	}
	return Item{
		Key:    "thumb:" + v.VideoID,
		Type:   "thumb",
		Source: v.ThumbnailURL,
		Path:   repo.ThumbPath(v.VideoID, v.ThumbnailURL),
	}, true
}

// AvatarItem builds the channel avatar item, keyed by handle so one avatar
// shared across a channel's videos is a single file.
func AvatarItem(c *youtube.Channel) (Item, bool) {
	if c == nil || c.AvatarURL == "" {
		return Item{}, false
	}
	handle := handleSeg(c)
	return Item{
		Key:    "avatar:" + handle,
		Type:   "avatar",
		Source: c.AvatarURL,
		Path:   repo.AvatarPath(handle, c.AvatarURL),
	}, true
}

// BannerItem builds the channel banner item.
func BannerItem(c *youtube.Channel) (Item, bool) {
	if c == nil || c.BannerURL == "" {
		return Item{}, false
	}
	handle := handleSeg(c)
	return Item{
		Key:    "banner:" + handle,
		Type:   "banner",
		Source: c.BannerURL,
		Path:   repo.BannerPath(handle, c.BannerURL),
	}, true
}

// CommentAvatarItem builds a download item for a comment author's avatar, keyed
// and named by the source URL so the same author across many comments resolves
// to one file. name labels the file segment for readability.
func CommentAvatarItem(name, srcURL string) (Item, bool) {
	if srcURL == "" {
		return Item{}, false
	}
	seg := name
	if seg == "" {
		seg = "author"
	}
	return Item{
		Key:    "cavatar:" + srcURL,
		Type:   "avatar",
		Source: srcURL,
		Path:   repo.AvatarPath(seg, srcURL),
	}, true
}

func handleSeg(c *youtube.Channel) string {
	h := strings.TrimPrefix(c.Handle, "@")
	if h != "" {
		return h
	}
	return c.ChannelID
}

// streamFormatToken renders a stable, filesystem-safe token for a selection so
// two different format choices for one video coexist on disk.
func streamFormatToken(sel youtube.Selection) string {
	var tags []string
	for _, s := range sel.Streams() {
		tags = append(tags, itag(s.ITag))
	}
	if len(tags) == 0 {
		return "stream"
	}
	return strings.Join(tags, "+")
}

func itag(n int) string {
	if n <= 0 {
		return "x"
	}
	return strconv.Itoa(n)
}
