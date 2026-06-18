package repo

import (
	"os"
	"path/filepath"
	"testing"
)

func TestStateRoundTrip(t *testing.T) {
	dir := t.TempDir()
	s := &State{
		Target:    TargetRef{Kind: "channel", Ref: "@mkbhd"},
		Depth:     "media",
		Videos:    42,
		NewestID:  "newest1234",
		OldestID:  "oldest5678",
		NewestAt:  "2026-06-18T00:00:00Z",
		OldestAt:  "2010-01-01T00:00:00Z",
		Complete:  true,
		UpdatedAt: "2026-06-18T01:00:00Z",
	}
	if err := s.Save(dir); err != nil {
		t.Fatalf("save: %v", err)
	}

	got, ok, err := LoadState(dir)
	if err != nil || !ok {
		t.Fatalf("load: ok=%v err=%v", ok, err)
	}
	if got.Schema != StateSchema {
		t.Errorf("schema = %d, want %d", got.Schema, StateSchema)
	}
	if got.NewestID != "newest1234" || got.Videos != 42 || !got.Complete {
		t.Errorf("round-trip mismatch: %+v", got)
	}
	// The atomic write leaves no temporary behind.
	if _, err := os.Stat(filepath.Join(dir, StateFile+".tmp")); !os.IsNotExist(err) {
		t.Errorf("a .tmp file should not survive a successful save")
	}
}

func TestLoadStateMissing(t *testing.T) {
	_, ok, err := LoadState(t.TempDir())
	if err != nil {
		t.Fatalf("missing state should not error: %v", err)
	}
	if ok {
		t.Fatal("ok should be false for a missing state file")
	}
}

func TestLoadStateCorruptIsAbsent(t *testing.T) {
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, StateFile), []byte("{not json"), 0o644); err != nil {
		t.Fatal(err)
	}
	_, ok, err := LoadState(dir)
	if err != nil {
		t.Fatalf("a corrupt cursor should degrade to absent, not error: %v", err)
	}
	if ok {
		t.Fatal("a corrupt state file should read as absent so a re-walk recovers")
	}
}
