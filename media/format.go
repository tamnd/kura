package media

import (
	"fmt"
	"strconv"
	"strings"
)

// videoBases are the format selectors that resolve to a video-bearing stream, so
// a height cap applies to them. Audio selectors (ba, ba*, bestaudio, m4a, ...) and
// explicit itags are left alone: audio has no height, and an itag is an exact
// choice the user already made.
var videoBases = map[string]bool{
	"best": true, "b": true, "worst": true, "w": true,
	"bestvideo": true, "bv": true, "bv*": true,
	"worstvideo": true, "wv": true,
	"mp4": true, "webm": true,
}

// capHeight rewrites a yt-dlp-grammar format spec so every video-bearing selector
// is capped at max pixels tall, by appending a [height<=max] filter to it. A
// selector that already constrains height, an audio selector, or an explicit itag
// is left untouched. With max <= 0 the spec is returned unchanged.
//
// The spec is a set of '/' fallback groups, each a '+' merge of selectors, each a
// base followed by zero or more [filter] terms. capHeight walks that structure and
// only rewrites the bases it recognises as video.
func capHeight(spec string, max int) string {
	if max <= 0 {
		return spec
	}
	groups := strings.Split(spec, "/")
	for gi, group := range groups {
		parts := strings.Split(group, "+")
		for pi, part := range parts {
			parts[pi] = capPart(part, max)
		}
		groups[gi] = strings.Join(parts, "+")
	}
	return strings.Join(groups, "/")
}

// capPart appends the height filter to one selector token when it is a video base
// that does not already filter on height.
func capPart(part string, max int) string {
	trimmed := strings.TrimSpace(part)
	base := trimmed
	if b, _, ok := strings.Cut(trimmed, "["); ok {
		base = b
	}
	if _, err := strconv.Atoi(strings.TrimSpace(base)); err == nil {
		return part // an explicit itag is an exact choice
	}
	if !videoBases[strings.TrimSpace(base)] {
		return part
	}
	if strings.Contains(trimmed, "height") {
		return part // already height-constrained
	}
	return trimmed + fmt.Sprintf("[height<=%d]", max)
}
