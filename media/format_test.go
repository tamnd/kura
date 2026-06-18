package media

import (
	"testing"

	"github.com/tamnd/ytb-cli/youtube"
)

func TestCapHeight(t *testing.T) {
	cases := []struct {
		spec string
		max  int
		want string
	}{
		{"bv*+ba/b", 0, "bv*+ba/b"}, // no cap
		{"bv*+ba/b", 720, "bv*[height<=720]+ba/b[height<=720]"},
		{"ba/ba*", 720, "ba/ba*"},   // audio selectors untouched
		{"137+140", 720, "137+140"}, // explicit itags untouched
		{"best", 480, "best[height<=480]"},
		{"bv*[height<=1080]", 720, "bv*[height<=1080]"}, // already constrained, left as-is
		{"bestvideo+bestaudio", 1080, "bestvideo[height<=1080]+bestaudio"},
	}
	for _, c := range cases {
		if got := capHeight(c.spec, c.max); got != c.want {
			t.Errorf("capHeight(%q, %d) = %q, want %q", c.spec, c.max, got, c.want)
		}
	}
}

// TestCapHeightSelects proves the capped spec actually steers the engine's
// selector to a rendition at or below the cap.
func TestCapHeightSelects(t *testing.T) {
	streams := []youtube.Stream{
		{ITag: 18, Container: "mp4", VideoCodec: "avc1", AudioCodec: "mp4a", Height: 360, Width: 640, FPS: 30, Bitrate: 500_000, HasVideo: true, HasAudio: true},
		{ITag: 22, Container: "mp4", VideoCodec: "avc1", AudioCodec: "mp4a", Height: 720, Width: 1280, FPS: 30, Bitrate: 2_000_000, HasVideo: true, HasAudio: true},
		{ITag: 137, Container: "mp4", VideoCodec: "avc1", Height: 1080, Width: 1920, FPS: 30, Bitrate: 4_000_000, HasVideo: true, IsAdaptive: true},
	}
	// Uncapped, the best progressive is 720 here; capped at 480 it must drop to 360.
	sel, err := youtube.SelectFormat(streams, capHeight("b", 480))
	if err != nil {
		t.Fatalf("SelectFormat: %v", err)
	}
	if sel.Video == nil || sel.Video.Height != 360 {
		t.Fatalf("cap 480 selected %+v, want the 360 rendition", sel.Video)
	}
}
