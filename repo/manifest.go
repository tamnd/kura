package repo

import (
	"encoding/json"
	"os"
	"path/filepath"
	"sort"
	"time"
)

// SchemaVersion is bumped when the on-disk repository shape changes, so a future
// kura can migrate an older archive.
const SchemaVersion = 1

// Manifest is the index of a repository: what was captured, how deep, how much,
// over what range, and where the holes are. It is written byte-deterministically
// (sorted lists, the only wall-clock value the per-capture stamp) so a re-run
// produces an identical file (KR5).
type Manifest struct {
	Service     string      `json:"service"` // always "youtube"
	Target      TargetRef   `json:"target"`
	Depth       string      `json:"depth"` // meta | media | audio
	Videos      int         `json:"videos"`
	Media       MediaCounts `json:"media"`
	Transcripts int         `json:"transcripts"`
	Comments    bool        `json:"comments_captured"`
	Range       Range       `json:"range"`
	Captures    []Capture   `json:"captures"`
	Gaps        []Gap       `json:"gaps,omitempty"`
	MediaIndex  []Asset     `json:"media_index,omitempty"`
	KuraVersion string      `json:"kura_version"`
	Schema      int         `json:"schema"`
}

// MediaCounts summarises how many of each media kind are on disk.
type MediaCounts struct {
	Thumbs int `json:"thumbs"`
	Videos int `json:"videos"`
	Audio  int `json:"audio"`
}

// TargetRef is the captured target's canonical identity.
type TargetRef struct {
	Kind      string `json:"kind"`
	Ref       string `json:"ref"`
	ChannelID string `json:"channel_id,omitempty"`
	Query     string `json:"query,omitempty"`
}

// Range is the published-time span of the captured videos.
type Range struct {
	Oldest time.Time `json:"oldest,omitempty"`
	Newest time.Time `json:"newest,omitempty"`
}

// Capture is one run against this repository. The At stamp is the only
// wall-clock value in the manifest and is the --date-overridable capture time.
type Capture struct {
	At    string `json:"at"`
	Added int    `json:"added"`
	Depth string `json:"depth"`
}

// Gap records a surface a capture could not reach: an IP-gated transcript or
// comment block, a stream with no progressive fallback, a failed fetch. The
// archive is honest about its holes (KR4).
type Gap struct {
	VideoID string `json:"video_id,omitempty"`
	What    string `json:"what"`
	Reason  string `json:"reason"`
}

// Asset is one localised media file (or a recorded failure to localise one).
type Asset struct {
	Key    string `json:"key"`
	Type   string `json:"type"`
	Path   string `json:"path,omitempty"`
	Source string `json:"source,omitempty"`
	Status string `json:"status"` // local | unavailable | stream-only | not-archived
}

// Asset statuses.
const (
	StatusLocal       = "local"
	StatusUnavailable = "unavailable"
	StatusStreamOnly  = "stream-only"
	StatusNotArchived = "not-archived"
)

// NewManifest builds an empty manifest for a fresh capture.
func NewManifest(target TargetRef, version string) *Manifest {
	return &Manifest{
		Service:     "youtube",
		Target:      target,
		KuraVersion: version,
		Schema:      SchemaVersion,
	}
}

// LoadManifest reads manifest.json from root. The bool is false (with no error)
// when the repository does not exist yet.
func LoadManifest(root string) (*Manifest, bool, error) {
	b, err := os.ReadFile(filepath.Join(root, ManifestFile))
	if os.IsNotExist(err) {
		return nil, false, nil
	}
	if err != nil {
		return nil, false, err
	}
	var m Manifest
	if err := json.Unmarshal(b, &m); err != nil {
		return nil, false, err
	}
	return &m, true, nil
}

// Save writes the manifest deterministically: sorted media index and gaps,
// indented JSON, trailing newline.
func (m *Manifest) Save(root string) error {
	m.normalize()
	b, err := json.MarshalIndent(m, "", "  ")
	if err != nil {
		return err
	}
	b = append(b, '\n')
	if err := os.MkdirAll(root, 0o755); err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(root, ManifestFile), b, 0o644)
}

// AddCapture appends a capture entry.
func (m *Manifest) AddCapture(at string, added int, depth string) {
	m.Captures = append(m.Captures, Capture{At: at, Added: added, Depth: depth})
}

// AddGap records a hole, de-duplicating identical entries.
func (m *Manifest) AddGap(videoID, what, reason string) {
	for _, g := range m.Gaps {
		if g.VideoID == videoID && g.What == what {
			return
		}
	}
	m.Gaps = append(m.Gaps, Gap{VideoID: videoID, What: what, Reason: reason})
}

func (m *Manifest) normalize() {
	if m.Schema == 0 {
		m.Schema = SchemaVersion
	}
	sort.Slice(m.MediaIndex, func(i, j int) bool {
		if m.MediaIndex[i].Key != m.MediaIndex[j].Key {
			return m.MediaIndex[i].Key < m.MediaIndex[j].Key
		}
		return m.MediaIndex[i].Source < m.MediaIndex[j].Source
	})
	sort.Slice(m.Gaps, func(i, j int) bool {
		if m.Gaps[i].VideoID != m.Gaps[j].VideoID {
			return m.Gaps[i].VideoID < m.Gaps[j].VideoID
		}
		return m.Gaps[i].What < m.Gaps[j].What
	})
}
