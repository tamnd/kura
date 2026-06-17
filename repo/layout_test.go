package repo

import "testing"

func TestVideoPaths(t *testing.T) {
	id := "dQw4w9WgXcQ"
	cases := map[string]string{
		VideoJSON(id):     "videos/dQw4w9WgXcQ.json",
		VideoRaw(id):      "videos/dQw4w9WgXcQ.raw.json",
		VideoComments(id): "videos/dQw4w9WgXcQ.comments.json",
		VideoChapters(id): "videos/dQw4w9WgXcQ.chapters.json",
		VideoSponsor(id):  "videos/dQw4w9WgXcQ.sponsorblock.json",
		VideoHTML(id):     "html/dQw4w9WgXcQ.html",
		VideoMD(id):       "md/dQw4w9WgXcQ.md",
	}
	for got, want := range cases {
		if got != want {
			t.Errorf("path = %q, want %q", got, want)
		}
	}
}

func TestTranscriptPaths(t *testing.T) {
	id := "abc"
	if got := TranscriptVTT(id, ""); got != "videos/abc.transcript.auto.vtt" {
		t.Errorf("default-lang VTT = %q", got)
	}
	if got := TranscriptVTT(id, "en"); got != "videos/abc.transcript.en.vtt" {
		t.Errorf("en VTT = %q", got)
	}
	if got := TranscriptTXT(id, "EN"); got != "videos/abc.transcript.en.txt" {
		t.Errorf("lang is lowercased: %q", got)
	}
}

func TestMediaPathsAreDeterministic(t *testing.T) {
	a := ThumbPath("vid", "https://i.ytimg.com/x.jpg")
	b := ThumbPath("vid", "https://i.ytimg.com/x.jpg")
	if a != b {
		t.Fatalf("thumb path not stable: %q vs %q", a, b)
	}
	if c := ThumbPath("vid", "https://i.ytimg.com/y.jpg"); c == a {
		t.Fatalf("distinct sources collide: %q", c)
	}
}

func TestVideoMediaPathExtension(t *testing.T) {
	if got := VideoMediaPath("id", "137+140", "mp4"); got != "media/video/id__137_140.mp4" {
		t.Errorf("video media path = %q", got)
	}
	if got := AudioMediaPath("id", "140", "m4a"); got != "media/audio/id__140.m4a" {
		t.Errorf("audio media path = %q", got)
	}
}

func TestRel(t *testing.T) {
	cases := []struct{ from, to, want string }{
		{"index.html", "html/abc.html", "html/abc.html"},
		{"html/abc.html", "index.html", "../index.html"},
		{"html/abc.html", "media/thumb/x.jpg", "../media/thumb/x.jpg"},
		{"html/abc.html", "html/def.html", "def.html"},
		{"md/abc.md", "_assets/kura.css", "../_assets/kura.css"},
	}
	for _, c := range cases {
		if got := Rel(c.from, c.to); got != c.want {
			t.Errorf("Rel(%q,%q) = %q, want %q", c.from, c.to, got, c.want)
		}
	}
}

func TestSafeSegEscapesTraversal(t *testing.T) {
	if got := safeSeg("../../etc/passwd"); got != "etc_passwd" {
		t.Errorf("safeSeg traversal = %q", got)
	}
	if got := safeSeg("@mkbhd"); got != "@mkbhd" {
		t.Errorf("handle preserved: %q", got)
	}
	if got := safeSeg(""); got != "item" {
		t.Errorf("empty fallback = %q", got)
	}
}
