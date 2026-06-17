// Package html renders the kage-shape static site from stored records: an inert,
// self-contained set of pages that look like a YouTube watch page and open with
// the network unplugged (spec §9). No <script>, no on* handlers, no remote fonts,
// no analytics — a photograph of the content that, at media depth, also plays.
// Templates are embedded so the binary needs no asset directory at runtime, and
// html/template auto-escapes every value.
package html

import (
	"bytes"
	"embed"
	"html/template"

	"github.com/tamnd/kura/render"
	"github.com/tamnd/kura/repo"
	"github.com/tamnd/ytb-cli/youtube"
)

//go:embed templates/*.tmpl
var templatesFS embed.FS

//go:embed assets/kura.css
var cssBytes []byte

// CSS returns the embedded stylesheet bytes so the caller can write it into the
// repository's _assets directory.
func CSS() []byte { return cssBytes }

var tmpl = template.Must(template.New("kura").Funcs(template.FuncMap{
	"count": render.FormatCount,
}).ParseFS(templatesFS, "templates/*.tmpl"))

// Renderer holds the shared context for one repository's pages. It is single-use
// per render pass and not safe for concurrent pages (it sets the per-page
// FromPage on the shared context before each build).
type Renderer struct {
	ctx     *render.Context
	channel *youtube.Channel
	footer  string
	navTit  string
}

// New builds a renderer over a record set, the localised media, and the captured
// channel (nil for a single-video or search archive). footer is the page footer
// line (carrying the --date capture stamp); navTitle is the repository's display
// name shown in the top nav.
func New(videos []*youtube.Video, assets []repo.Asset, channel *youtube.Channel, footer, navTitle string) *Renderer {
	return &Renderer{
		ctx:     render.NewContext(videos, assets),
		channel: channel,
		footer:  footer,
		navTit:  navTitle,
	}
}

type channelView struct {
	Name        string
	Handle      string
	Verified    bool
	Description string
	AvatarSrc   string
	BannerSrc   string
	Subscribers string
	Videos      string
}

type pageData struct {
	Title    string
	CSSHref  string
	HomeHref string
	NavTitle string
	Footer   string
	Channel  *channelView
	Heading  string
	SubHead  string
	Single   bool // a watch page (one video, full record)
	Card     render.VideoView
	Cards    []render.VideoView // the index grid
}

// VideoPage renders one video as a watch page at html/<id>.html.
func (r *Renderer) VideoPage(b render.Bundle) (string, error) {
	if b.Video == nil {
		return "", nil
	}
	page := repo.VideoHTML(b.Video.VideoID)
	r.ctx.FromPage = page
	v := r.ctx.Build(b)
	r.ctx.SetAvatar(&v, r.channel)
	data := pageData{
		Title:    pageTitle(v),
		CSSHref:  repo.Rel(page, repo.CSSFile),
		HomeHref: repo.Rel(page, repo.IndexHTML),
		NavTitle: r.navTit,
		Footer:   r.footer,
		Single:   true,
		Card:     v,
	}
	return r.exec(data)
}

// Index renders the repository home at index.html: the channel header (when a
// channel was captured) followed by a grid of the captured videos in the order
// given, each linking to its page. heading/subheading label a non-channel
// capture (a search or a playlist).
func (r *Renderer) Index(bundles []render.Bundle, heading, subheading string) (string, error) {
	page := repo.IndexHTML
	r.ctx.FromPage = page
	cards := make([]render.VideoView, 0, len(bundles))
	for _, b := range bundles {
		v := r.ctx.Build(b)
		r.ctx.SetAvatar(&v, r.channel)
		cards = append(cards, v)
	}
	data := pageData{
		Title:    r.navTit,
		CSSHref:  repo.Rel(page, repo.CSSFile),
		HomeHref: ".",
		NavTitle: r.navTit,
		Footer:   r.footer,
		Heading:  heading,
		SubHead:  subheading,
		Cards:    cards,
	}
	if r.channel != nil {
		data.Channel = r.channelView(page)
	}
	return r.exec(data)
}

func (r *Renderer) channelView(page string) *channelView {
	c := r.channel
	name := c.Title
	if name == "" {
		name = c.Handle
	}
	cv := &channelView{
		Name:        name,
		Handle:      handleOf(c),
		Verified:    c.IsVerified,
		Description: c.Description,
		Subscribers: subsText(c),
		Videos:      c.VideosText,
	}
	if src, ok := r.ctx.MediaSrc(c.AvatarURL); ok {
		cv.AvatarSrc = src
	} else if c.AvatarURL != "" {
		cv.AvatarSrc = c.AvatarURL
	}
	if src, ok := r.ctx.MediaSrc(c.BannerURL); ok {
		cv.BannerSrc = src
	} else if c.BannerURL != "" {
		cv.BannerSrc = c.BannerURL
	}
	return cv
}

func (r *Renderer) exec(data pageData) (string, error) {
	var buf bytes.Buffer
	if err := tmpl.ExecuteTemplate(&buf, "layout", data); err != nil {
		return "", err
	}
	return buf.String(), nil
}

func pageTitle(v render.VideoView) string {
	if v.Title == "" {
		return "Video " + v.ID
	}
	if v.ChannelName != "" {
		return v.Title + " — " + v.ChannelName
	}
	return v.Title
}

func handleOf(c *youtube.Channel) string {
	h := c.Handle
	if h == "" {
		return ""
	}
	if h[0] == '@' {
		return h[1:]
	}
	return h
}

func subsText(c *youtube.Channel) string {
	if c.SubscribersText != "" {
		return c.SubscribersText
	}
	if c.SubscriberCount > 0 {
		return render.FormatCount(c.SubscriberCount) + " subscribers"
	}
	return ""
}
