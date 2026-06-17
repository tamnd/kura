// Package md renders the yomi-shape Markdown archive: a plain-text mirror of the
// repository that reads top to bottom, greps, and diffs (spec §10). The repo home
// is README.md; each video is md/<id>.md. Markdown derives from the same stored
// records and the same view model as the HTML site (KR3), so the two views always
// agree. Output is deterministic — no clock, no map iteration in the text — so
// golden tests run with no network. The full transcript is written inline, which
// is the single most valuable thing to have in greppable Markdown: a channel's
// entire spoken content becomes full-text searchable on disk.
package md

import (
	"fmt"
	"strings"

	"github.com/tamnd/kura/render"
	"github.com/tamnd/kura/repo"
	"github.com/tamnd/ytb-cli/youtube"
)

// Renderer builds the Markdown views over one repository's records and media.
type Renderer struct {
	ctx     *render.Context
	channel *youtube.Channel
	footer  string
	title   string
}

// New builds a Markdown renderer. footer carries the capture stamp; title is the
// repository display name used in the README heading; channel is the captured
// channel (nil for a single-video or search archive).
func New(videos []*youtube.Video, assets []repo.Asset, channel *youtube.Channel, footer, title string) *Renderer {
	return &Renderer{
		ctx:     render.NewContext(videos, assets),
		channel: channel,
		footer:  footer,
		title:   title,
	}
}

// Video renders one video as a standalone Markdown document for md/<id>.md.
func (r *Renderer) Video(b render.Bundle) string {
	if b.Video == nil {
		return ""
	}
	page := repo.VideoMD(b.Video.VideoID)
	r.ctx.FromPage = page
	v := r.ctx.Build(b)
	r.ctx.SetAvatar(&v, r.channel)

	var sb strings.Builder
	r.writeFrontMatter(&sb, v)
	fmt.Fprintf(&sb, "# %s\n\n", mdEscape(v.Title))

	if v.ThumbSrc != "" {
		fmt.Fprintf(&sb, "![%s](%s)\n\n", mdEscape(v.Title), v.ThumbSrc)
	}

	// Channel line and a link to the source on YouTube.
	chan_ := v.ChannelName
	if chan_ != "" {
		if v.ChannelRel != "" {
			fmt.Fprintf(&sb, "**[%s](%s)**", mdEscape(chan_), v.ChannelRel)
		} else {
			fmt.Fprintf(&sb, "**%s**", mdEscape(chan_))
		}
		sb.WriteString(" · ")
	}
	fmt.Fprintf(&sb, "[watch on YouTube](%s)\n\n", v.URL)

	metrics := metricsLine(v)
	if metrics != "" {
		sb.WriteString(metrics)
		sb.WriteString("\n\n")
	}

	jump := render.MarkdownJump(v.ID, v.URL, v.HasStream())
	if body := strings.TrimSpace(render.LinkifyMarkdown(v.TextBody, jump)); body != "" {
		sb.WriteString(body)
		sb.WriteString("\n\n")
	}

	r.writeChapters(&sb, v)
	r.writeTranscript(&sb, v)
	r.writeComments(&sb, v)

	r.writeFooter(&sb, repo.Rel(page, repo.ReadmeMD))
	return sb.String()
}

// Index renders README.md: a front-matter block, the channel header (when
// captured), and a table of contents linking each video to its file in the order
// given. heading/subheading label a non-channel capture.
func (r *Renderer) Index(bundles []render.Bundle, heading, subheading string) string {
	page := repo.ReadmeMD
	r.ctx.FromPage = page
	var sb strings.Builder

	r.writeIndexFrontMatter(&sb, len(bundles))

	if r.channel != nil {
		r.writeChannel(&sb)
	} else {
		fmt.Fprintf(&sb, "# %s\n\n", mdEscape(r.title))
	}
	if heading != "" {
		fmt.Fprintf(&sb, "## %s\n\n", mdEscape(heading))
		if subheading != "" {
			fmt.Fprintf(&sb, "%s\n\n", mdEscape(subheading))
		}
	}
	fmt.Fprintf(&sb, "%d videos archived.\n\n", len(bundles))
	for _, b := range bundles {
		v := r.ctx.Build(b)
		link := repo.Rel(page, repo.VideoMD(v.ID))
		stamp := v.Stamp
		if stamp == "" {
			stamp = "—"
		}
		fmt.Fprintf(&sb, "- [%s](%s) — %s\n", stamp, link, summary(v.Title))
	}
	sb.WriteString("\n")
	sb.WriteString(r.footer)
	sb.WriteString("\n")
	return sb.String()
}

func (r *Renderer) writeFrontMatter(sb *strings.Builder, v render.VideoView) {
	sb.WriteString("---\n")
	fmt.Fprintf(sb, "video_id: %s\n", v.ID)
	fmt.Fprintf(sb, "title: %s\n", yamlString(v.Title))
	if v.ChannelName != "" {
		fmt.Fprintf(sb, "channel: %s\n", yamlString(v.ChannelName))
	}
	fmt.Fprintf(sb, "url: %s\n", v.URL)
	if v.Stamp != "" {
		fmt.Fprintf(sb, "published: %s\n", v.Stamp)
	}
	if v.Duration != "" {
		fmt.Fprintf(sb, "duration: %s\n", yamlString(v.Duration))
	}
	if v.Views > 0 {
		fmt.Fprintf(sb, "views: %d\n", v.Views)
	}
	if v.Likes > 0 {
		fmt.Fprintf(sb, "likes: %d\n", v.Likes)
	}
	sb.WriteString("---\n\n")
}

func (r *Renderer) writeIndexFrontMatter(sb *strings.Builder, count int) {
	sb.WriteString("---\n")
	fmt.Fprintf(sb, "service: youtube\n")
	fmt.Fprintf(sb, "title: %s\n", yamlString(r.title))
	fmt.Fprintf(sb, "videos: %d\n", count)
	sb.WriteString("---\n\n")
}

func (r *Renderer) writeChannel(sb *strings.Builder) {
	c := r.channel
	name := c.Title
	if name == "" {
		name = c.Handle
	}
	handle := strings.TrimPrefix(c.Handle, "@")
	if handle != "" {
		fmt.Fprintf(sb, "# %s (@%s)\n\n", mdEscape(name), handle)
	} else {
		fmt.Fprintf(sb, "# %s\n\n", mdEscape(name))
	}
	stats := []string{}
	if c.SubscribersText != "" {
		stats = append(stats, c.SubscribersText)
	} else if c.SubscriberCount > 0 {
		stats = append(stats, render.FormatCount(c.SubscriberCount)+" subscribers")
	}
	if c.VideosText != "" {
		stats = append(stats, c.VideosText)
	}
	if len(stats) > 0 {
		fmt.Fprintf(sb, "%s\n\n", strings.Join(stats, " · "))
	}
	if c.Description != "" {
		jump := render.MarkdownJump("", "", false)
		fmt.Fprintf(sb, "%s\n\n", render.LinkifyMarkdown(c.Description, jump))
	}
}

func (r *Renderer) writeChapters(sb *strings.Builder, v render.VideoView) {
	if len(v.Chapters) == 0 {
		return
	}
	sb.WriteString("## Chapters\n\n")
	for _, ch := range v.Chapters {
		fmt.Fprintf(sb, "- [%s](%s) %s\n", ch.Offset, ch.Jump, mdEscape(ch.Title))
	}
	sb.WriteString("\n")
}

func (r *Renderer) writeTranscript(sb *strings.Builder, v render.VideoView) {
	if len(v.Transcript) == 0 {
		return
	}
	heading := "Transcript"
	if v.TransLang != "" {
		heading = fmt.Sprintf("Transcript (%s)", v.TransLang)
	}
	fmt.Fprintf(sb, "## %s\n\n", heading)
	for _, s := range v.Transcript {
		fmt.Fprintf(sb, "`%s` %s\n\n", s.Offset, s.Text)
	}
}

func (r *Renderer) writeComments(sb *strings.Builder, v render.VideoView) {
	if len(v.Comments) == 0 {
		return
	}
	sb.WriteString("## Comments\n\n")
	for _, c := range v.Comments {
		indent := ""
		if c.IsReply {
			indent = "  "
		}
		owner := ""
		if c.IsOwner {
			owner = " ★"
		}
		body := strings.TrimSpace(strings.ReplaceAll(c.TextBody, "\n", " "))
		fmt.Fprintf(sb, "%s- **%s**%s", indent, mdEscape(c.Author), owner)
		if c.Likes > 0 {
			fmt.Fprintf(sb, " (♥ %s)", render.FormatCount(c.Likes))
		}
		fmt.Fprintf(sb, ": %s\n", mdEscape(body))
	}
	sb.WriteString("\n")
}

func (r *Renderer) writeFooter(sb *strings.Builder, homeRel string) {
	fmt.Fprintf(sb, "---\n\n[← archive home](%s)\n\n%s\n", homeRel, r.footer)
}

func metricsLine(v render.VideoView) string {
	parts := []string{}
	if v.Views > 0 {
		parts = append(parts, render.FormatCount(v.Views)+" views")
	}
	if v.Stamp != "" {
		parts = append(parts, v.Stamp)
	}
	if v.Likes > 0 {
		parts = append(parts, "♥ "+render.FormatCount(v.Likes))
	}
	if v.Duration != "" {
		parts = append(parts, v.Duration)
	}
	return strings.Join(parts, " · ")
}

// summary trims a title to a single short line for the index list.
func summary(text string) string {
	text = strings.ReplaceAll(text, "\n", " ")
	text = strings.Join(strings.Fields(text), " ")
	if len([]rune(text)) > 100 {
		text = string([]rune(text)[:100]) + "…"
	}
	if text == "" {
		return "(untitled)"
	}
	return mdEscape(text)
}

// mdEscape neutralises the few characters that would break inline Markdown when
// they appear in a name, label, or title. Descriptions are left to LinkifyMarkdown,
// which keeps them readable.
func mdEscape(s string) string {
	r := strings.NewReplacer(
		"[", "\\[", "]", "\\]",
		"*", "\\*", "_", "\\_",
		"`", "\\`", "|", "\\|",
		"<", "&lt;", ">", "&gt;",
	)
	return r.Replace(s)
}

// yamlString quotes a front-matter scalar when it could be misread as YAML
// structure, keeping the block parseable.
func yamlString(s string) string {
	s = strings.ReplaceAll(s, "\n", " ")
	if s == "" {
		return `""`
	}
	if strings.ContainsAny(s, ":#\"'{}[]&*!|>%@`") || strings.HasPrefix(s, " ") || strings.HasSuffix(s, " ") {
		return `"` + strings.ReplaceAll(s, `"`, `\"`) + `"`
	}
	return s
}
