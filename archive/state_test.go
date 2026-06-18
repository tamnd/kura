package archive

import (
	"testing"
	"time"

	"github.com/tamnd/ytb-cli/youtube"
)

func TestRangeIDs(t *testing.T) {
	at := func(s string) time.Time {
		tm, err := time.Parse(time.RFC3339, s)
		if err != nil {
			t.Fatal(err)
		}
		return tm
	}
	all := []*youtube.Video{
		{VideoID: "mid", PublishedAt: at("2015-06-01T00:00:00Z")},
		{VideoID: "newest", PublishedAt: at("2026-01-01T00:00:00Z")},
		{VideoID: "nodate"}, // zero time, ignored
		{VideoID: "oldest", PublishedAt: at("2008-01-01T00:00:00Z")},
	}
	newestID, oldestID, newestAt, oldestAt := rangeIDs(all)
	if newestID != "newest" {
		t.Errorf("newestID = %q, want newest", newestID)
	}
	if oldestID != "oldest" {
		t.Errorf("oldestID = %q, want oldest", oldestID)
	}
	if newestAt != "2026-01-01T00:00:00Z" {
		t.Errorf("newestAt = %q", newestAt)
	}
	if oldestAt != "2008-01-01T00:00:00Z" {
		t.Errorf("oldestAt = %q", oldestAt)
	}
}

func TestRangeIDsEmpty(t *testing.T) {
	newestID, oldestID, newestAt, oldestAt := rangeIDs(nil)
	if newestID != "" || oldestID != "" || newestAt != "" || oldestAt != "" {
		t.Errorf("empty input should yield empty cursor, got %q/%q/%q/%q", newestID, oldestID, newestAt, oldestAt)
	}
}
