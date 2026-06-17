package archive

import "testing"

func TestParseTargetKinds(t *testing.T) {
	cases := []struct {
		arg  string
		sel  Selector
		kind Kind
		ref  string
	}{
		{arg: "dQw4w9WgXcQ", kind: KindVideo, ref: "dQw4w9WgXcQ"},
		{arg: "https://www.youtube.com/watch?v=dQw4w9WgXcQ", kind: KindVideo, ref: "dQw4w9WgXcQ"},
		{arg: "@mkbhd", kind: KindChannel, ref: "@mkbhd"},
		{arg: "UCBJycsmduvYEL83R_U4JriQ", kind: KindChannel, ref: "UCBJycsmduvYEL83R_U4JriQ"},
		{arg: "https://www.youtube.com/@mkbhd/videos", kind: KindChannel, ref: "@mkbhd"},
		{arg: "PLrAXtmErZgOeiKm4sgNOknGvNjby9efdf", kind: KindPlaylist, ref: "PLrAXtmErZgOeiKm4sgNOknGvNjby9efdf"},
		{sel: Selector{Search: "lofi mix"}, kind: KindSearch, ref: "lofi mix"},
		{sel: Selector{Playlist: "PLxxxx"}, kind: KindPlaylist, ref: "PLxxxx"},
	}
	for _, c := range cases {
		got, err := ParseTarget(c.arg, c.sel)
		if err != nil {
			t.Errorf("ParseTarget(%q): %v", c.arg, err)
			continue
		}
		if got.Kind != c.kind || got.Ref != c.ref {
			t.Errorf("ParseTarget(%q) = {%s %q}, want {%s %q}", c.arg, got.Kind, got.Ref, c.kind, c.ref)
		}
	}
}

func TestParseTargetErrors(t *testing.T) {
	if _, err := ParseTarget("", Selector{}); err == nil {
		t.Error("empty target should error")
	}
	if _, err := ParseTarget("x", Selector{Search: "a", Playlist: "b"}); err == nil {
		t.Error("conflicting selectors should error")
	}
}

func TestTargetRoot(t *testing.T) {
	cases := []struct {
		t    Target
		want string
	}{
		{Target{Kind: KindChannel, Ref: "@mkbhd"}, "out/youtube/@mkbhd"},
		{Target{Kind: KindVideo, Ref: "dQw4w9WgXcQ"}, "out/youtube/video-dqw4w9wgxcq"},
		{Target{Kind: KindSearch, Ref: "Lofi Mix"}, "out/youtube/search-lofi-mix"},
		{Target{Kind: KindPlaylist, Ref: "PLxxxx"}, "out/youtube/playlist-plxxxx"},
	}
	for _, c := range cases {
		if got := c.t.Root("out"); got != c.want {
			t.Errorf("Root for %s/%q = %q, want %q", c.t.Kind, c.t.Ref, got, c.want)
		}
	}
}

func TestTargetRefManifest(t *testing.T) {
	r := Target{Kind: KindChannel, Ref: "UCBJycsmduvYEL83R_U4JriQ"}.TargetRef()
	if r.ChannelID != "UCBJycsmduvYEL83R_U4JriQ" {
		t.Errorf("UC ref should populate ChannelID: %+v", r)
	}
	s := Target{Kind: KindSearch, Ref: "cats"}.TargetRef()
	if s.Query != "cats" {
		t.Errorf("search ref should populate Query: %+v", s)
	}
}
