package render

import (
	"strings"
	"testing"

	"github.com/tamnd/kura/repo"
	"github.com/tamnd/ytb-cli/youtube"
)

func sampleBundle() Bundle {
	return Bundle{
		Video: &youtube.Video{
			VideoID:      "vid123",
			Title:        "A <Great> Video",
			Description:  "See https://example.com and 1:23 for the good part #cool",
			ChannelID:    "UCchan",
			ChannelName:  "The Channel",
			ViewCount:    1500000,
			LikeCount:    2300,
			URL:          "https://www.youtube.com/watch?v=vid123",
			ThumbnailURL: "https://t/vid123.jpg",
		},
		Chapters: []youtube.Chapter{
			{Title: "Intro", StartSeconds: 0},
			{Title: "Middle", StartSeconds: 83},
		},
		Comments: []youtube.Comment{
			{ID: "c1", AuthorDisplayName: "Alice", TextDisplay: "nice 1:23", LikeCount: 5},
		},
		Transcript: []youtube.TranscriptSegment{
			{StartSeconds: 0, Text: "hello world"},
			{StartSeconds: 83, Text: "the good part"},
		},
		TransLang: "en",
	}
}

func sampleAssets() []repo.Asset {
	return []repo.Asset{
		{Key: "thumb:vid123", Type: "thumb", Source: "https://t/vid123.jpg", Path: "media/thumb/vid123__abc.jpg", Status: repo.StatusLocal},
		{Key: "video:vid123", Type: "video", Source: "https://www.youtube.com/watch?v=vid123", Path: "media/video/vid123__137.mp4", Status: repo.StatusLocal},
	}
}

func TestContextBuildResolvesLocalStreamAndThumb(t *testing.T) {
	ctx := NewContext([]*youtube.Video{sampleBundle().Video}, sampleAssets())
	ctx.FromPage = repo.VideoHTML("vid123")
	v := ctx.Build(sampleBundle())

	if !v.HasStream() {
		t.Error("video with a local stream asset should report HasStream")
	}
	if !strings.HasPrefix(v.StreamSrc, "../media/video/") {
		t.Errorf("stream src not page-relative: %q", v.StreamSrc)
	}
	if !v.ThumbLocal || !strings.Contains(v.ThumbSrc, "media/thumb/") {
		t.Errorf("thumb not resolved to local: local=%v src=%q", v.ThumbLocal, v.ThumbSrc)
	}
	if len(v.Chapters) != 2 || v.Chapters[1].Offset != "1:23" {
		t.Errorf("chapters wrong: %+v", v.Chapters)
	}
	// A local stream makes chapter jumps fragment links.
	if !strings.HasPrefix(v.Chapters[1].Jump, "#t=") {
		t.Errorf("chapter jump should be a local fragment: %q", v.Chapters[1].Jump)
	}
}

func TestBuildEscapesAndLinkifies(t *testing.T) {
	ctx := NewContext(nil, nil)
	v := ctx.Build(sampleBundle())
	body := string(v.HTMLBody)
	if strings.Contains(body, "<Great>") {
		t.Error("title-like angle brackets in description must be escaped")
	}
	if !strings.Contains(body, `href="https://example.com"`) {
		t.Errorf("URL not linkified: %q", body)
	}
	if !strings.Contains(body, "hashtag/cool") {
		t.Errorf("hashtag not linkified: %q", body)
	}
}

func TestFormatCount(t *testing.T) {
	cases := map[int64]string{
		999:        "999",
		1500:       "1.5K",
		2300000:    "2.3M",
		1000000000: "1B",
	}
	for in, want := range cases {
		if got := FormatCount(in); got != want {
			t.Errorf("FormatCount(%d) = %q, want %q", in, got, want)
		}
	}
}
