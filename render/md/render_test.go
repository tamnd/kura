package md

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
			Title:       "A Great Video",
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

func TestVideoMarkdownContent(t *testing.T) {
	b := sampleBundle()
	r := New([]*youtube.Video{b.Video}, nil, nil, "footer line", "Archive")
	out := r.Video(b)
	for _, want := range []string{
		"# A Great Video", // heading
		"https://example.com",
		"Intro",         // chapter
		"the good part", // transcript
		"Alice",         // comment
		"footer line",
	} {
		if !strings.Contains(out, want) {
			t.Errorf("Video markdown missing %q", want)
		}
	}
	if !strings.HasPrefix(out, "---\n") {
		t.Errorf("markdown must open with YAML front matter, got: %.40q", out)
	}
}

func TestVideoMarkdownDeterministic(t *testing.T) {
	b := sampleBundle()
	r := New([]*youtube.Video{b.Video}, nil, nil, "f", "Archive")
	first, second := r.Video(b), r.Video(b)
	if first != second {
		t.Error("Video markdown is not deterministic")
	}
}

func TestIndexMarkdown(t *testing.T) {
	b := sampleBundle()
	r := New([]*youtube.Video{b.Video}, nil, nil, "f", "Archive")
	out := r.Index([]render.Bundle{b}, "The Channel", "1 video")
	if !strings.Contains(out, "The Channel") {
		t.Errorf("index markdown missing heading: %q", out)
	}
}
