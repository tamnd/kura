package repo

import (
	"os"
	"testing"
)

func TestManifestRoundTrip(t *testing.T) {
	dir := t.TempDir()
	mf := NewManifest(TargetRef{Kind: "channel", Ref: "@mkbhd"}, "v1.2.3")
	mf.Videos = 3
	mf.AddCapture("2026-06-18T00:00:00Z", 3, "meta")
	if err := mf.Save(dir); err != nil {
		t.Fatalf("save: %v", err)
	}

	got, ok, err := LoadManifest(dir)
	if err != nil || !ok {
		t.Fatalf("load: ok=%v err=%v", ok, err)
	}
	if got.Videos != 3 || got.Target.Ref != "@mkbhd" || got.KuraVersion != "v1.2.3" {
		t.Errorf("round-trip mismatch: %+v", got)
	}
	if got.Schema != SchemaVersion {
		t.Errorf("schema = %d, want %d", got.Schema, SchemaVersion)
	}
}

func TestLoadManifestMissing(t *testing.T) {
	_, ok, err := LoadManifest(t.TempDir())
	if err != nil {
		t.Fatalf("missing manifest should not error: %v", err)
	}
	if ok {
		t.Fatal("ok should be false for a missing manifest")
	}
}

func TestAddGapDedupes(t *testing.T) {
	mf := NewManifest(TargetRef{}, "v1")
	mf.AddGap("vid", "comments", "hidden")
	mf.AddGap("vid", "comments", "hidden again")
	if len(mf.Gaps) != 1 {
		t.Fatalf("expected 1 gap after dedupe, got %d", len(mf.Gaps))
	}
	mf.AddGap("vid", "transcript", "gated")
	if len(mf.Gaps) != 2 {
		t.Fatalf("distinct what should add: got %d", len(mf.Gaps))
	}
}

func TestSaveIsDeterministic(t *testing.T) {
	build := func() *Manifest {
		mf := NewManifest(TargetRef{Kind: "channel", Ref: "@x"}, "v1")
		mf.MediaIndex = []Asset{
			{Key: "thumb:b", Type: "thumb", Status: StatusLocal},
			{Key: "thumb:a", Type: "thumb", Status: StatusLocal},
		}
		mf.AddGap("z", "comments", "x")
		mf.AddGap("a", "transcript", "y")
		mf.AddCapture("2026-06-18T00:00:00Z", 1, "meta")
		return mf
	}
	d1, d2 := t.TempDir(), t.TempDir()
	if err := build().Save(d1); err != nil {
		t.Fatal(err)
	}
	if err := build().Save(d2); err != nil {
		t.Fatal(err)
	}
	b1, _ := os.ReadFile(d1 + "/" + ManifestFile)
	b2, _ := os.ReadFile(d2 + "/" + ManifestFile)
	if string(b1) != string(b2) {
		t.Fatalf("manifest save is not deterministic:\n%s\n---\n%s", b1, b2)
	}
}
