package media

import (
	"testing"

	"github.com/tamnd/ytb-cli/youtube"
)

func TestParseDepth(t *testing.T) {
	cases := map[string]Depth{
		"":        DepthMeta,
		"meta":    DepthMeta,
		"MEDIA":   DepthMedia,
		" audio ": DepthAudio,
	}
	for in, want := range cases {
		if got, ok := ParseDepth(in); !ok || got != want {
			t.Errorf("ParseDepth(%q) = %q, %v; want %q", in, got, ok, want)
		}
	}
	if _, ok := ParseDepth("nonsense"); ok {
		t.Error("nonsense depth should be rejected")
	}
}

func TestWantsStream(t *testing.T) {
	if DepthMeta.WantsStream() {
		t.Error("meta should not want a stream")
	}
	if !DepthMedia.WantsStream() || !DepthAudio.WantsStream() {
		t.Error("media and audio depths should want a stream")
	}
}

func TestThumbItem(t *testing.T) {
	if _, ok := ThumbItem(&youtube.Video{VideoID: "x"}); ok {
		t.Error("no thumbnail URL should yield no item")
	}
	it, ok := ThumbItem(&youtube.Video{VideoID: "x", ThumbnailURL: "https://t/x.jpg"})
	if !ok {
		t.Fatal("expected a thumb item")
	}
	if it.Key != "thumb:x" || it.Source != "https://t/x.jpg" || it.Type != "thumb" {
		t.Errorf("thumb item = %+v", it)
	}
}

func TestCommentAvatarItemKeyedBySource(t *testing.T) {
	a, _ := CommentAvatarItem("Alice", "https://a/pic.jpg")
	b, _ := CommentAvatarItem("Alice again", "https://a/pic.jpg")
	if a.Key != b.Key {
		t.Errorf("same source should share a key: %q vs %q", a.Key, b.Key)
	}
	if _, ok := CommentAvatarItem("x", ""); ok {
		t.Error("empty source should yield no item")
	}
}

func TestPlanImagesDedupesAndSorts(t *testing.T) {
	ch := &youtube.Channel{ChannelID: "UC1", AvatarURL: "https://a/av.jpg", BannerURL: "https://a/ban.jpg"}
	vids := []*youtube.Video{
		{VideoID: "v2", ThumbnailURL: "https://t/v2.jpg"},
		{VideoID: "v1", ThumbnailURL: "https://t/v1.jpg"},
		{VideoID: "v1", ThumbnailURL: "https://t/v1.jpg"}, // duplicate destination
		{VideoID: "v3"}, // no thumb, skipped
	}
	items := PlanImages(ch, vids)
	// avatar + banner + two distinct thumbs = 4 (the duplicate v1 collapses).
	if len(items) != 4 {
		t.Fatalf("planned %d items, want 4: %+v", len(items), items)
	}
	for i := 1; i < len(items); i++ {
		if items[i-1].Path >= items[i].Path {
			t.Errorf("items not sorted by path: %q !< %q", items[i-1].Path, items[i].Path)
		}
	}
}

func TestPlanImagesNilChannel(t *testing.T) {
	items := PlanImages(nil, []*youtube.Video{{VideoID: "v1", ThumbnailURL: "https://t/v1.jpg"}})
	if len(items) != 1 {
		t.Fatalf("nil channel should still plan video thumbs: %+v", items)
	}
}

func TestStreamFormatToken(t *testing.T) {
	sel := youtube.Selection{
		Video: &youtube.Stream{ITag: 137},
		Audio: &youtube.Stream{ITag: 140},
	}
	if got := streamFormatToken(sel); got != "137+140" {
		t.Errorf("token = %q, want 137+140", got)
	}
	if got := streamFormatToken(youtube.Selection{}); got != "stream" {
		t.Errorf("empty selection token = %q", got)
	}
}
