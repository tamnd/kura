package repo

import (
	"encoding/json"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/tamnd/ytb-cli/youtube"
)

// Store is a handle on one repository directory. Writes are atomic (temp file
// then rename) so an interrupted run never leaves a half-written record, and a
// resumed run finds clean files (KR6).
type Store struct {
	dir string
}

// Open returns a Store rooted at dir, creating the directory if needed.
func Open(dir string) (*Store, error) {
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return nil, err
	}
	return &Store{dir: dir}, nil
}

// Dir returns the repository root.
func (s *Store) Dir() string { return s.dir }

// WriteVideo writes the canonical video record and, when raw is non-nil, the
// fuller engine payload beside it.
func (s *Store) WriteVideo(v *youtube.Video, raw any) error {
	if err := s.WriteJSON(VideoJSON(v.VideoID), v); err != nil {
		return err
	}
	if raw != nil {
		if err := s.WriteJSON(VideoRaw(v.VideoID), raw); err != nil {
			return err
		}
	}
	return nil
}

// WriteChannel writes the channel record.
func (s *Store) WriteChannel(c *youtube.Channel) error { return s.WriteJSON(ChannelJSON, c) }

// WriteJSON marshals v (indented) to a repository-relative path.
func (s *Store) WriteJSON(rel string, v any) error {
	b, err := json.MarshalIndent(v, "", "  ")
	if err != nil {
		return err
	}
	return s.writeFile(rel, append(b, '\n'))
}

// WriteText writes a text file (HTML, Markdown, CSS, transcript) to a
// repository-relative path.
func (s *Store) WriteText(rel, body string) error { return s.writeFile(rel, []byte(body)) }

// WriteMedia writes binary media to a repository-relative path.
func (s *Store) WriteMedia(rel string, data []byte) error { return s.writeFile(rel, data) }

// Abs resolves a repository-relative path to an absolute one.
func (s *Store) Abs(rel string) string {
	return filepath.Join(s.dir, filepath.FromSlash(rel))
}

// Exists reports whether a repository-relative path is present.
func (s *Store) Exists(rel string) bool {
	_, err := os.Stat(s.Abs(rel))
	return err == nil
}

// HasVideo reports whether a video's canonical record is already stored.
func (s *Store) HasVideo(id string) bool { return s.Exists(VideoJSON(id)) }

// LoadVideo reads one canonical video record by id.
func (s *Store) LoadVideo(id string) (*youtube.Video, error) {
	b, err := os.ReadFile(s.Abs(VideoJSON(id)))
	if err != nil {
		return nil, err
	}
	var v youtube.Video
	if err := json.Unmarshal(b, &v); err != nil {
		return nil, err
	}
	return &v, nil
}

// LoadVideos reads every canonical video record under videos/, sorted by id so
// the order is stable across reads (KR5).
func (s *Store) LoadVideos() ([]*youtube.Video, error) {
	dir := filepath.Join(s.dir, DirVideos)
	entries, err := os.ReadDir(dir)
	if os.IsNotExist(err) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var ids []string
	for _, e := range entries {
		name := e.Name()
		if e.IsDir() || !strings.HasSuffix(name, ".json") {
			continue
		}
		if strings.HasSuffix(name, ".raw.json") || strings.HasSuffix(name, ".comments.json") ||
			strings.HasSuffix(name, ".chapters.json") || strings.HasSuffix(name, ".sponsorblock.json") {
			continue
		}
		ids = append(ids, strings.TrimSuffix(name, ".json"))
	}
	sort.Strings(ids)
	out := make([]*youtube.Video, 0, len(ids))
	for _, id := range ids {
		v, err := s.LoadVideo(id)
		if err != nil {
			return nil, err
		}
		out = append(out, v)
	}
	return out, nil
}

// LoadChannel reads the channel record. The bool is false (no error) when the
// repository has no channel record (e.g. a single-video or search archive).
func (s *Store) LoadChannel() (*youtube.Channel, bool, error) {
	b, err := os.ReadFile(s.Abs(ChannelJSON))
	if os.IsNotExist(err) {
		return nil, false, nil
	}
	if err != nil {
		return nil, false, err
	}
	var c youtube.Channel
	if err := json.Unmarshal(b, &c); err != nil {
		return nil, false, err
	}
	return &c, true, nil
}

// LoadChapters reads the chapter sidecar for a video, if present.
func (s *Store) LoadChapters(id string) ([]youtube.Chapter, error) {
	return loadJSONSlice[youtube.Chapter](s, VideoChapters(id))
}

// LoadComments reads the comment sidecar for a video, if present.
func (s *Store) LoadComments(id string) ([]youtube.Comment, error) {
	return loadJSONSlice[youtube.Comment](s, VideoComments(id))
}

// LoadSponsor reads the SponsorBlock sidecar for a video, if present.
func (s *Store) LoadSponsor(id string) ([]youtube.SponsorSegment, error) {
	return loadJSONSlice[youtube.SponsorSegment](s, VideoSponsor(id))
}

func loadJSONSlice[T any](s *Store, rel string) ([]T, error) {
	b, err := os.ReadFile(s.Abs(rel))
	if os.IsNotExist(err) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var out []T
	if err := json.Unmarshal(b, &out); err != nil {
		return nil, err
	}
	return out, nil
}

// writeFile creates parent directories then writes atomically.
func (s *Store) writeFile(rel string, data []byte) error {
	abs := s.Abs(rel)
	if err := os.MkdirAll(filepath.Dir(abs), 0o755); err != nil {
		return err
	}
	tmp := abs + ".tmp"
	if err := os.WriteFile(tmp, data, 0o644); err != nil {
		return err
	}
	return os.Rename(tmp, abs)
}
