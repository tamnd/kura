package repo

import (
	"encoding/json"
	"os"
	"path/filepath"
)

// StateSchema is the version of the state.json shape, bumped on a breaking change.
const StateSchema = 1

// State is the resume anchor for a repository: a small, separate record of the
// last capture's cursor, written next to the manifest. It lets an `add` on an
// existing channel or playlist do a fast incremental forward update (page only
// what is newer than the newest already held) instead of re-walking, while an
// interrupted backfill re-walks to finish. It is advisory: deleting it only costs
// a re-walk, never data, since the records on disk remain the source of truth.
type State struct {
	Schema   int       `json:"schema"`
	Target   TargetRef `json:"target"`
	Depth    string    `json:"depth"`
	Videos   int       `json:"videos"`
	NewestID string    `json:"newest_id,omitempty"`
	OldestID string    `json:"oldest_id,omitempty"`
	NewestAt string    `json:"newest_at,omitempty"`
	OldestAt string    `json:"oldest_at,omitempty"`
	// Complete is true when the last capture streamed the spine to its natural end
	// (or reached its incremental boundary), so the backfill is exhaustive and a
	// resume can page only newer uploads. A false value means an interrupted or
	// budget-stopped run, and a resume re-walks to finish.
	Complete  bool   `json:"complete"`
	UpdatedAt string `json:"updated_at"`
}

// LoadState reads state.json from root. The bool is false (with no error) when no
// state has been written yet, or when the file is unreadable as state (treated as
// absent so a corrupt cursor degrades to a re-walk rather than an error).
func LoadState(root string) (*State, bool, error) {
	b, err := os.ReadFile(filepath.Join(root, StateFile))
	if os.IsNotExist(err) {
		return nil, false, nil
	}
	if err != nil {
		return nil, false, err
	}
	var s State
	if err := json.Unmarshal(b, &s); err != nil {
		return nil, false, nil
	}
	return &s, true, nil
}

// Save writes state.json atomically: a temporary sibling is written and renamed
// into place, so a crash mid-write never leaves a torn resume cursor.
func (s *State) Save(root string) error {
	s.Schema = StateSchema
	b, err := json.MarshalIndent(s, "", "  ")
	if err != nil {
		return err
	}
	b = append(b, '\n')
	if err := os.MkdirAll(root, 0o755); err != nil {
		return err
	}
	final := filepath.Join(root, StateFile)
	tmp := final + ".tmp"
	if err := os.WriteFile(tmp, b, 0o644); err != nil {
		return err
	}
	return os.Rename(tmp, final)
}
