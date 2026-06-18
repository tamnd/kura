package archive

import (
	"fmt"
	"path"
	"strings"

	"github.com/tamnd/kura/repo"
	"github.com/tamnd/ytb-cli/youtube"
)

// Kind is what a capture target points at (spec §7).
type Kind string

const (
	KindVideo    Kind = "video"
	KindChannel  Kind = "channel"
	KindPlaylist Kind = "playlist"
	KindSearch   Kind = "search"
)

// Target is a parsed, canonical capture target. Ref addresses the source through
// the engine (a video id, a channel handle/id, a playlist id, or a search query);
// Display is the human label for the page nav and headings.
type Target struct {
	Kind    Kind
	Ref     string
	Display string
}

// Selector carries the flags that pick a non-default target kind, so a bare
// argument is read in the light of --search/--album (spec §12.1). At most one may
// be set.
type Selector struct {
	Search   string
	Playlist string // --album / --playlist
}

// ParseTarget resolves a CLI argument and the kind-selecting flags into a
// canonical Target. The grammar is the engine's URL grammar plus kura's keywords:
// a watch URL or 11-char id is a video, an @handle/channel URL/UC id is a channel,
// a PL id or playlist URL is a playlist, and --search/--album select the query
// kinds. arg may be empty when a flag fully specifies the target (--search).
func ParseTarget(arg string, sel Selector) (Target, error) {
	if sel.Search != "" && sel.Playlist != "" {
		return Target{}, fmt.Errorf("choose only one of --search/--album")
	}
	switch {
	case sel.Search != "":
		q := strings.TrimSpace(sel.Search)
		return Target{Kind: KindSearch, Ref: q, Display: "Search: " + q}, nil
	case sel.Playlist != "":
		id := strings.TrimSpace(sel.Playlist)
		return Target{Kind: KindPlaylist, Ref: id, Display: "Playlist " + id}, nil
	}

	arg = strings.TrimSpace(arg)
	if arg == "" {
		return Target{}, fmt.Errorf("a target is required (a video id or URL, an @handle or channel URL, a playlist id, or --search/--album)")
	}

	// A channel reference wins over a video id so a UC… id or an @handle is never
	// misread as a video.
	if looksChannel(arg) {
		return Target{Kind: KindChannel, Ref: channelRef(arg), Display: channelDisplay(arg)}, nil
	}
	// A bare playlist id wins over the video check: the engine's video matcher is
	// permissive and would otherwise read a 34-char PL… token as a video id.
	if looksPlaylist(arg) {
		if plid := youtube.ExtractPlaylistID(arg); plid != "" {
			return Target{Kind: KindPlaylist, Ref: plid, Display: "Playlist " + plid}, nil
		}
	}
	// A watch URL carries v=; a bare 11-char token is a video id. The engine's
	// ExtractVideoID is permissive — it echoes any bare string back as an id, so a
	// vanity name like "mkbhd" would be misread as a video. Gate it: a URL form
	// goes to the engine, but a bare token must be an exact 11-character id to be a
	// video, otherwise it falls through to the channel-handle case below.
	if id := videoID(arg); id != "" {
		return Target{Kind: KindVideo, Ref: id, Display: "Video " + id}, nil
	}
	if plid := youtube.ExtractPlaylistID(arg); plid != "" {
		return Target{Kind: KindPlaylist, Ref: plid, Display: "Playlist " + plid}, nil
	}
	// Fall back to treating the argument as a channel handle.
	return Target{Kind: KindChannel, Ref: channelRef(arg), Display: channelDisplay(arg)}, nil
}

// videoID returns the video id for arg, or "" when arg is not a video. A
// URL-shaped argument (carrying a scheme, path, or query) is handed to the
// engine's permissive matcher, which understands watch, youtu.be, /shorts/, and
// /embed/ forms. A bare token must match the exact 11-character YouTube id shape;
// anything shorter or longer (a vanity channel name) is not a video.
func videoID(arg string) string {
	if strings.ContainsAny(arg, ":/?=") || strings.Contains(arg, "youtu") {
		return youtube.ExtractVideoID(arg)
	}
	if isVideoID(arg) {
		return arg
	}
	return ""
}

// isVideoID reports whether s is exactly an 11-character YouTube video id: the
// base64url alphabet, no separators.
func isVideoID(s string) bool {
	if len(s) != 11 {
		return false
	}
	for _, r := range s {
		switch {
		case r >= 'A' && r <= 'Z', r >= 'a' && r <= 'z', r >= '0' && r <= '9', r == '_', r == '-':
		default:
			return false
		}
	}
	return true
}

// looksChannel reports whether an argument is unmistakably a channel reference.
func looksChannel(s string) bool {
	if strings.HasPrefix(s, "@") {
		return true
	}
	if strings.HasPrefix(s, "UC") && len(s) == 24 {
		return true
	}
	for _, m := range []string{"/channel/", "/c/", "/user/", "/@", "youtube.com/@"} {
		if strings.Contains(s, m) {
			return true
		}
	}
	return false
}

// looksPlaylist reports whether a bare argument (no scheme) is a playlist id.
// Playlist ids carry a known two-letter prefix and run well past a video id's 11
// characters, so the prefix plus length distinguishes them from a watch token.
func looksPlaylist(s string) bool {
	if strings.Contains(s, "list=") {
		return true
	}
	if strings.ContainsAny(s, "/:?&") || len(s) <= 11 {
		return false
	}
	for _, p := range []string{"PL", "UU", "LL", "FL", "RD", "OL", "PU"} {
		if strings.HasPrefix(s, p) {
			return true
		}
	}
	return false
}

// channelRef reduces a channel argument to the canonical reference the engine
// accepts: a bare @handle, a UC id, or the URL itself.
func channelRef(s string) string {
	if i := strings.Index(s, "/@"); i >= 0 {
		h := s[i+1:]
		if j := strings.IndexAny(h, "/?#"); j >= 0 {
			h = h[:j]
		}
		return h
	}
	return s
}

// channelDisplay is the human label for a channel target.
func channelDisplay(s string) string {
	ref := channelRef(s)
	if strings.HasPrefix(ref, "@") {
		return ref
	}
	return "Channel " + ref
}

// Root returns the repository directory for this target under out:
// out/youtube/<slug>, where <slug> is the canonical, filesystem-safe target
// identity (spec §6.1). Two captures of the same target land in the same repo and
// merge. A channel keeps its @handle so the path reads youtube/@mkbhd.
func (t Target) Root(out string) string {
	return path.Join(out, "youtube", t.slug())
}

func (t Target) slug() string {
	switch t.Kind {
	case KindChannel:
		return channelSlug(t.Ref)
	case KindVideo:
		return "video-" + safeName(t.Ref)
	case KindPlaylist:
		return "playlist-" + safeName(t.Ref)
	case KindSearch:
		return "search-" + safeName(t.Ref)
	default:
		return safeName(t.Ref)
	}
}

// channelSlug keeps a leading @handle intact (youtube/@mkbhd), else uses the safe
// channel id.
func channelSlug(ref string) string {
	if h, ok := strings.CutPrefix(ref, "@"); ok {
		return "@" + safeName(h)
	}
	return safeName(ref)
}

// TargetRef converts the parsed target into the manifest's record of what the
// repository archives.
func (t Target) TargetRef() repo.TargetRef {
	r := repo.TargetRef{Kind: string(t.Kind), Ref: t.Ref}
	switch t.Kind {
	case KindSearch:
		r.Query = t.Ref
	case KindChannel:
		if strings.HasPrefix(t.Ref, "UC") && len(t.Ref) == 24 {
			r.ChannelID = t.Ref
		}
	}
	return r
}

// safeName reduces an arbitrary ref to one safe, compact, lowercase path segment.
func safeName(s string) string {
	s = strings.TrimSpace(strings.ToLower(s))
	var b strings.Builder
	for _, r := range s {
		switch {
		case r >= 'a' && r <= 'z', r >= '0' && r <= '9', r == '_' || r == '-':
			b.WriteRune(r)
		case r == ' ' || r == ':' || r == '/' || r == '#' || r == '.':
			b.WriteByte('-')
		}
	}
	out := strings.Trim(b.String(), "-")
	if out == "" {
		return "capture"
	}
	if len(out) > 60 {
		out = strings.Trim(out[:60], "-")
	}
	return out
}
