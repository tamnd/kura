package repo

import (
	"testing"

	"github.com/tamnd/ytb-cli/youtube"
)

func TestStoreVideoRoundTrip(t *testing.T) {
	st, err := Open(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	v := &youtube.Video{VideoID: "abc", Title: "Hello", ChannelName: "Chan"}
	if err := st.WriteVideo(v, map[string]any{"raw": true}); err != nil {
		t.Fatal(err)
	}
	if !st.HasVideo("abc") {
		t.Fatal("HasVideo should be true after write")
	}
	got, err := st.LoadVideo("abc")
	if err != nil {
		t.Fatal(err)
	}
	if got.Title != "Hello" {
		t.Errorf("title = %q", got.Title)
	}
	if !st.Exists(VideoRaw("abc")) {
		t.Error("raw payload not written")
	}
}

func TestLoadVideosSortedAndFiltered(t *testing.T) {
	st, _ := Open(t.TempDir())
	for _, id := range []string{"ccc", "aaa", "bbb"} {
		if err := st.WriteVideo(&youtube.Video{VideoID: id}, nil); err != nil {
			t.Fatal(err)
		}
	}
	// A sidecar must not be mistaken for a record.
	if err := st.WriteJSON(VideoComments("aaa"), []youtube.Comment{{ID: "c"}}); err != nil {
		t.Fatal(err)
	}
	all, err := st.LoadVideos()
	if err != nil {
		t.Fatal(err)
	}
	if len(all) != 3 {
		t.Fatalf("expected 3 records, got %d", len(all))
	}
	if all[0].VideoID != "aaa" || all[2].VideoID != "ccc" {
		t.Errorf("not sorted by id: %v", []string{all[0].VideoID, all[1].VideoID, all[2].VideoID})
	}
}

func TestLoadSidecarsAbsentIsNotError(t *testing.T) {
	st, _ := Open(t.TempDir())
	if cs, err := st.LoadComments("missing"); err != nil || cs != nil {
		t.Errorf("missing comments: cs=%v err=%v", cs, err)
	}
	if chs, err := st.LoadChapters("missing"); err != nil || chs != nil {
		t.Errorf("missing chapters: chs=%v err=%v", chs, err)
	}
}

func TestLoadChannelAbsent(t *testing.T) {
	st, _ := Open(t.TempDir())
	_, ok, err := st.LoadChannel()
	if err != nil {
		t.Fatalf("absent channel should not error: %v", err)
	}
	if ok {
		t.Fatal("ok should be false with no channel record")
	}
}
