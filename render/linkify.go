package render

import (
	"html"
	"html/template"
	"regexp"
	"strconv"
	"strings"
)

// reEntity recognises, in one pass over the raw (unescaped) text, the surface
// features a YouTube description links: a URL, a timestamp (h:mm:ss or mm:ss), or
// a #hashtag. URL is first so a colon or hash inside a URL is not re-read as a
// timestamp or a tag. The text is matched raw, not HTML-escaped, so the numeric
// character references escaping introduces can never be mistaken for a hashtag.
// Each gap between matches is escaped as it is emitted and the anchors this file
// writes are the only markup the output carries, so the page stays inert (no
// script, no event handlers) per kage's posture.
var reEntity = regexp.MustCompile(`(https?://[^\s<]+)|(\b\d{1,2}:[0-5]?\d(?::[0-5]\d)?\b)|#(\w+)`)

// linkifyHTML turns a description (or a comment) into safe HTML: URLs become
// anchors, a timestamp becomes a jump link into the player, hashtags link to
// youtube, everything else is HTML-escaped, and newlines survive as text (the
// .body rule renders pre-wrap). It returns template.HTML because the output is
// already escaped and the template must emit it verbatim.
func linkifyHTML(text string, j jumpTarget) template.HTML {
	var b strings.Builder
	last := 0
	for _, m := range reEntity.FindAllStringSubmatchIndex(text, -1) {
		start, end := m[0], m[1]
		b.WriteString(html.EscapeString(text[last:start]))
		switch {
		case m[2] >= 0: // URL
			writeURLAnchor(&b, text[start:end])
		case m[4] >= 0: // timestamp
			ts := text[m[4]:m[5]]
			if sec, ok := parseClock(ts); ok && j.url != "" {
				b.WriteString(`<a href="` + html.EscapeString(j.at(sec)) + `">` + html.EscapeString(ts) + `</a>`)
			} else {
				b.WriteString(html.EscapeString(ts))
			}
		case m[6] >= 0: // hashtag
			tag := text[m[6]:m[7]]
			b.WriteString(`<a href="https://www.youtube.com/hashtag/` + tag + `" rel="nofollow noopener">#` + tag + `</a>`)
		}
		last = end
	}
	b.WriteString(html.EscapeString(text[last:]))
	return template.HTML(b.String())
}

// writeURLAnchor emits a URL as an anchor, keeping trailing punctuation outside
// the link and escaping the URL for both the href and the visible text.
func writeURLAnchor(b *strings.Builder, u string) {
	u, trail := splitTrail(u)
	esc := html.EscapeString(u)
	b.WriteString(`<a href="` + esc + `" rel="nofollow noopener">` + esc + `</a>`)
	b.WriteString(html.EscapeString(trail))
}

// LinkifyMarkdown turns the same surface features into Markdown: URLs become
// autolinks, a timestamp becomes a jump link, hashtags link to youtube. The text
// is otherwise left verbatim so it reads naturally and greps.
func LinkifyMarkdown(text string, j jumpTarget) string {
	var b strings.Builder
	last := 0
	for _, m := range reEntity.FindAllStringSubmatchIndex(text, -1) {
		start, end := m[0], m[1]
		b.WriteString(text[last:start])
		switch {
		case m[2] >= 0:
			u, trail := splitTrail(text[start:end])
			b.WriteString("[" + u + "](" + u + ")" + trail)
		case m[4] >= 0:
			ts := text[m[4]:m[5]]
			if sec, ok := parseClock(ts); ok && j.url != "" {
				b.WriteString("[" + ts + "](" + j.at(sec) + ")")
			} else {
				b.WriteString(ts)
			}
		case m[6] >= 0:
			tag := text[m[6]:m[7]]
			b.WriteString("[#" + tag + "](https://www.youtube.com/hashtag/" + tag + ")")
		}
		last = end
	}
	b.WriteString(text[last:])
	return b.String()
}

// MarkdownJump exposes a jump target to the md renderer so a description links
// timestamps into the same player the HTML page uses.
func MarkdownJump(vid, url string, local bool) jumpTarget {
	return jumpTarget{vid: vid, url: url, local: local}
}

// splitTrail peels trailing sentence punctuation off a URL so it stays outside
// the link.
func splitTrail(u string) (string, string) {
	trail := ""
	for len(u) > 0 {
		last := u[len(u)-1]
		if strings.IndexByte(").,!?;:", last) >= 0 {
			trail = string(last) + trail
			u = u[:len(u)-1]
			continue
		}
		break
	}
	return u, trail
}

// parseClock converts an h:mm:ss or mm:ss label to a second offset.
func parseClock(s string) (int, bool) {
	parts := strings.Split(s, ":")
	if len(parts) < 2 || len(parts) > 3 {
		return 0, false
	}
	nums := make([]int, len(parts))
	for i, p := range parts {
		n, err := strconv.Atoi(p)
		if err != nil {
			return 0, false
		}
		nums[i] = n
	}
	if len(parts) == 2 {
		return nums[0]*60 + nums[1], true
	}
	return nums[0]*3600 + nums[1]*60 + nums[2], true
}
