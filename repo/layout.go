// Package repo defines the on-disk shape of a kura archive: where every record,
// sidecar, media file, and rendered view lives, and how the manifest indexes
// them. Every path is a deterministic function of the record it holds (KR5), so
// a re-capture lands on the same files and the repository diffs cleanly.
package repo

import (
	"crypto/sha256"
	"encoding/hex"
	"path"
	"strings"
)

// Well-known files at the root of a repository.
const (
	ManifestFile = "manifest.json" // the index
	StateFile    = "state.json"    // capture cursors + download resume state
	ChannelJSON  = "channel.json"  // the captured Channel record
	IndexHTML    = "index.html"    // the repository home (HTML)
	ReadmeMD     = "README.md"     // the repository home (Markdown)
	CSSFile      = "_assets/kura.css"
)

// Top-level subdirectories.
const (
	DirVideos    = "videos"
	DirHTML      = "html"
	DirMD        = "md"
	DirPlaylists = "playlists"
	DirCommunity = "community"
	DirMedia     = "media"
)

// Media subdirectories, one per kind.
const (
	MediaThumb  = "media/thumb"
	MediaAvatar = "media/avatar"
	MediaBanner = "media/banner"
	MediaVideo  = "media/video"
	MediaAudio  = "media/audio"
)

// VideoJSON is the canonical record for a video: videos/<id>.json.
func VideoJSON(id string) string { return path.Join(DirVideos, safeSeg(id)+".json") }

// VideoRaw is the full engine payload for a video: videos/<id>.raw.json.
func VideoRaw(id string) string { return path.Join(DirVideos, safeSeg(id)+".raw.json") }

// VideoComments is the captured comment tree: videos/<id>.comments.json.
func VideoComments(id string) string { return path.Join(DirVideos, safeSeg(id)+".comments.json") }

// VideoChapters is the chapter list: videos/<id>.chapters.json.
func VideoChapters(id string) string { return path.Join(DirVideos, safeSeg(id)+".chapters.json") }

// VideoSponsor is the SponsorBlock segment list: videos/<id>.sponsorblock.json.
func VideoSponsor(id string) string { return path.Join(DirVideos, safeSeg(id)+".sponsorblock.json") }

// TranscriptVTT is the timed transcript: videos/<id>.transcript.<lang>.vtt.
func TranscriptVTT(id, lang string) string {
	return path.Join(DirVideos, safeSeg(id)+".transcript."+langSeg(lang)+".vtt")
}

// TranscriptTXT is the flat transcript: videos/<id>.transcript.<lang>.txt.
func TranscriptTXT(id, lang string) string {
	return path.Join(DirVideos, safeSeg(id)+".transcript."+langSeg(lang)+".txt")
}

// VideoHTML is the rendered watch page: html/<id>.html.
func VideoHTML(id string) string { return path.Join(DirHTML, safeSeg(id)+".html") }

// VideoMD is the rendered Markdown page: md/<id>.md.
func VideoMD(id string) string { return path.Join(DirMD, safeSeg(id)+".md") }

// PlaylistJSON is a captured playlist record: playlists/<plid>.json.
func PlaylistJSON(plid string) string { return path.Join(DirPlaylists, safeSeg(plid)+".json") }

// CommunityJSON is a captured community post: community/<postid>.json.
func CommunityJSON(postID string) string { return path.Join(DirCommunity, safeSeg(postID)+".json") }

// ThumbPath is the localised thumbnail for a video, keyed by id + source hash so
// two captures of the same thumbnail resolve to one file.
func ThumbPath(id, srcURL string) string {
	return path.Join(MediaThumb, safeSeg(id)+"__"+shortHash(srcURL)+".jpg")
}

// AvatarPath is the localised channel avatar, keyed by handle + source hash.
func AvatarPath(handle, srcURL string) string {
	return path.Join(MediaAvatar, safeSeg(handle)+"__"+shortHash(srcURL)+".jpg")
}

// BannerPath is the localised channel banner, keyed by handle + source hash.
func BannerPath(handle, srcURL string) string {
	return path.Join(MediaBanner, safeSeg(handle)+"__"+shortHash(srcURL)+".jpg")
}

// VideoMediaPath is the downloaded video stream for a video, keyed by id and the
// format token that produced it, so two selections coexist.
func VideoMediaPath(id, format, ext string) string {
	return path.Join(MediaVideo, safeSeg(id)+"__"+safeSeg(format)+normalizeExt(ext))
}

// AudioMediaPath is the downloaded audio stream for a video.
func AudioMediaPath(id, format, ext string) string {
	return path.Join(MediaAudio, safeSeg(id)+"__"+safeSeg(format)+normalizeExt(ext))
}

// Rel computes the path of `to` relative to the directory holding `from`, both
// repository-relative, so a rendered page can link to a sibling page or a media
// file with no absolute URL (KR2). Forward slashes only, since the result feeds
// HTML and Markdown.
func Rel(from, to string) string {
	from = path.Clean("/" + strings.ReplaceAll(from, "\\", "/"))
	to = path.Clean("/" + strings.ReplaceAll(to, "\\", "/"))
	if from == to {
		return path.Base(to)
	}
	fromDir := path.Dir(from)
	fs := splitNonEmpty(fromDir)
	ts := splitNonEmpty(path.Dir(to))
	// Drop the shared prefix.
	i := 0
	for i < len(fs) && i < len(ts) && fs[i] == ts[i] {
		i++
	}
	var seg []string
	for range fs[i:] {
		seg = append(seg, "..")
	}
	seg = append(seg, ts[i:]...)
	seg = append(seg, path.Base(to))
	return path.Join(seg...)
}

// shortHash is the first 6 hex of sha256(s): a stable, collision-resistant tag
// that keeps media filenames deterministic and inside their directory.
func shortHash(s string) string {
	sum := sha256.Sum256([]byte(s))
	return hex.EncodeToString(sum[:])[:6]
}

// safeSeg keeps a string safe as one path segment: alphanumerics, dot, dash,
// underscore, and @ (channel handles) survive; everything else becomes an
// underscore. Leading dots are trimmed so a segment can never escape upward.
func safeSeg(s string) string {
	var b strings.Builder
	for _, r := range s {
		switch {
		case r >= 'a' && r <= 'z', r >= 'A' && r <= 'Z', r >= '0' && r <= '9':
			b.WriteRune(r)
		case r == '.' || r == '-' || r == '_' || r == '@':
			b.WriteRune(r)
		default:
			b.WriteByte('_')
		}
	}
	out := collapseRuns(b.String(), '_')
	out = strings.Trim(out, "._-")
	if out == "" {
		return "item"
	}
	return out
}

// collapseRuns squeezes consecutive occurrences of r into a single one.
func collapseRuns(s string, r byte) string {
	var b strings.Builder
	var prev byte
	for i := 0; i < len(s); i++ {
		if s[i] == r && prev == r {
			continue
		}
		b.WriteByte(s[i])
		prev = s[i]
	}
	return b.String()
}

// langSeg normalises a transcript language label to a safe, lowercase segment,
// defaulting to "auto" when no language is known.
func langSeg(lang string) string {
	lang = strings.TrimSpace(strings.ToLower(lang))
	if lang == "" {
		return "auto"
	}
	return safeSeg(lang)
}

// normalizeExt returns a single safe, lowercase, dot-prefixed extension.
func normalizeExt(ext string) string {
	ext = strings.TrimSpace(strings.ToLower(ext))
	ext = strings.TrimPrefix(ext, ".")
	if ext == "" {
		ext = "bin"
	}
	return "." + safeSeg(ext)
}

func splitNonEmpty(p string) []string {
	var out []string
	for s := range strings.SplitSeq(p, "/") {
		if s != "" {
			out = append(out, s)
		}
	}
	return out
}
