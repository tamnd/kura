package html

import (
	"strings"
	"testing"

	"github.com/tamnd/kura/render"
	"github.com/tamnd/ytb-cli/youtube"
)

func sampleBundle() render.Bundle {
	return render.Bundle{
		Video: &youtube.Video{
			VideoID:     "vid123",
			Title:       "A <Great> Video",
			Description: "See https://example.com and 1:23 for the good part",
			ChannelID:   "UCchan",
			ChannelName: "The Channel",
			ViewCount:   1500000,
			URL:         "https://www.youtube.com/watch?v=vid123",
		},
		Chapters: []youtube.Chapter{
			{Title: "Intro", StartSeconds: 0},
			{Title: "Middle", StartSeconds: 83},
		},
		Comments: []youtube.Comment{
			{ID: "c1", AuthorDisplayName: "Alice", TextDisplay: "great stuff", LikeCount: 5},
		},
		Transcript: []youtube.TranscriptSegment{
			{StartSeconds: 0, Text: "hello world"},
			{StartSeconds: 83, Text: "the good part"},
		},
		TransLang: "en",
	}
}

func TestVideoPageContentAndEscaping(t *testing.T) {
	r := New([]*youtube.Video{sampleBundle().Video}, nil, nil, "footer line", "Archive")
	out, err := r.VideoPage(sampleBundle())
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{
		"A &lt;Great&gt; Video", // title escaped, not raw
		`href="https://example.com"`,
		"the good part", // transcript line present
		"Alice",         // comment author present
		"Intro",         // chapter present
		"footer line",
	} {
		if !strings.Contains(out, want) {
			t.Errorf("VideoPage output missing %q", want)
		}
	}
	if strings.Contains(out, "<Great>") {
		t.Error("raw unescaped title leaked into HTML")
	}
}

func TestVideoPageDeterministic(t *testing.T) {
	mk := func() string {
		r := New([]*youtube.Video{sampleBundle().Video}, nil, nil, "f", "Archive")
		out, err := r.VideoPage(sampleBundle())
		if err != nil {
			t.Fatal(err)
		}
		return out
	}
	first, second := mk(), mk()
	if first != second {
		t.Error("VideoPage is not deterministic across identical renders")
	}
}

func TestIndexListsVideos(t *testing.T) {
	b := sampleBundle()
	r := New([]*youtube.Video{b.Video}, nil, nil, "f", "Archive")
	out, err := r.Index([]render.Bundle{b}, "The Channel", "1 video")
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(out, "The Channel") || !strings.Contains(out, "1 video") {
		t.Errorf("index missing heading/subheading: %q", out)
	}
}
