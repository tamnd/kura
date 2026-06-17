package archive

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/tamnd/kura/repo"
	"github.com/tamnd/ytb-cli/youtube"
)

// seedRepo writes a minimal but complete repository to dir: a manifest, a channel
// record, and two video records with one sidecar each, so the no-network render
// path has real material to replay.
func seedRepo(t *testing.T, dir string) {
	t.Helper()
	st, err := repo.Open(dir)
	if err != nil {
		t.Fatal(err)
	}
	if err := st.WriteChannel(&youtube.Channel{ChannelID: "UCchan", Title: "The Channel", Handle: "@chan"}); err != nil {
		t.Fatal(err)
	}
	for _, v := range []*youtube.Video{
		{VideoID: "aaa11111111", Title: "First Video", ChannelID: "UCchan", ChannelName: "The Channel", Description: "watch https://example.com"},
		{VideoID: "bbb22222222", Title: "Second Video", ChannelID: "UCchan", ChannelName: "The Channel"},
	} {
		if err := st.WriteVideo(v, nil); err != nil {
			t.Fatal(err)
		}
	}
	// A comment sidecar for one video, to exercise loadBundle.
	if err := st.WriteJSON(repo.VideoComments("aaa11111111"), []youtube.Comment{
		{ID: "c1", AuthorDisplayName: "Alice", TextDisplay: "nice"},
	}); err != nil {
		t.Fatal(err)
	}

	mf := repo.NewManifest(repo.TargetRef{Kind: "channel", Ref: "@chan", ChannelID: "UCchan"}, "v0.1.0")
	mf.Depth = "meta"
	mf.Videos = 2
	mf.AddCapture("2026-06-18T00:00:00Z", 2, "meta")
	if err := mf.Save(dir); err != nil {
		t.Fatal(err)
	}
}

func TestRenderProducesViews(t *testing.T) {
	dir := t.TempDir()
	seedRepo(t, dir)

	res, err := Render(dir, RenderOptions{Views: []string{"html", "md"}, Version: "v0.1.0"})
	if err != nil {
		t.Fatal(err)
	}
	if res.Videos != 2 {
		t.Errorf("rendered %d videos, want 2", res.Videos)
	}

	mustExist := []string{
		repo.IndexHTML,
		filepath.FromSlash(repo.VideoHTML("aaa11111111")),
		filepath.FromSlash(repo.VideoHTML("bbb22222222")),
		filepath.FromSlash(repo.VideoMD("aaa11111111")),
		"README.md",
	}
	for _, rel := range mustExist {
		if _, err := os.Stat(filepath.Join(dir, rel)); err != nil {
			t.Errorf("expected rendered file %s: %v", rel, err)
		}
	}

	// The watch page must carry the title and the linkified description.
	page, err := os.ReadFile(filepath.Join(dir, filepath.FromSlash(repo.VideoHTML("aaa11111111"))))
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{"First Video", `href="https://example.com"`, "Alice"} {
		if !strings.Contains(string(page), want) {
			t.Errorf("watch page missing %q", want)
		}
	}
}

func TestRenderIsDeterministic(t *testing.T) {
	read := func() []byte {
		dir := t.TempDir()
		seedRepo(t, dir)
		if _, err := Render(dir, RenderOptions{Views: []string{"html"}, Version: "v0.1.0"}); err != nil {
			t.Fatal(err)
		}
		b, err := os.ReadFile(filepath.Join(dir, filepath.FromSlash(repo.VideoHTML("aaa11111111"))))
		if err != nil {
			t.Fatal(err)
		}
		return b
	}
	first, second := read(), read()
	if string(first) != string(second) {
		t.Error("render output is not deterministic across two fresh repos")
	}
}

func TestRenderRejectsNonRepo(t *testing.T) {
	if _, err := Render(t.TempDir(), RenderOptions{}); err == nil {
		t.Error("rendering a directory with no manifest should error")
	}
}
