package media

import (
	"context"
	"fmt"
	"io"
	"net/http"
	"sort"

	"github.com/tamnd/kura/repo"
	"github.com/tamnd/ytb-cli/youtube"
)

// ImageResult summarises an image localisation pass for the manifest and the
// progress line.
type ImageResult struct {
	Assets     []repo.Asset
	Downloaded int // newly fetched this run
	Reused     int // already on disk, skipped
	Failed     int // fetch errored, recorded unavailable
}

// PlanImages collects every distinct image to localise for a channel and its
// videos, deduped by destination path and ordered deterministically (KR5). It is
// pure: choosing what to fetch needs no network. Comment author avatars are added
// separately by the archive layer (it holds the comment trees), via
// CommentAvatarItem.
func PlanImages(channel *youtube.Channel, videos []*youtube.Video) []Item {
	byPath := map[string]Item{}
	add := func(it Item, ok bool) {
		if !ok || it.Source == "" || it.Path == "" {
			return
		}
		if _, seen := byPath[it.Path]; !seen {
			byPath[it.Path] = it
		}
	}
	add(AvatarItem(channel))
	add(BannerItem(channel))
	for _, v := range videos {
		add(ThumbItem(v))
	}
	return sortItems(byPath)
}

func sortItems(byPath map[string]Item) []Item {
	out := make([]Item, 0, len(byPath))
	for _, it := range byPath {
		out = append(out, it)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Path < out[j].Path })
	return out
}

// DownloadImages fetches each planned image through the engine's HTTP client so
// it rides the same transport as the records. An item already on disk is skipped
// (incremental, KR6); a fetch failure is recorded as unavailable and never aborts
// the capture (KR4). The asset's Source is preserved exactly so the renderers
// resolve a poster or avatar back to its local file by source URL.
func DownloadImages(ctx context.Context, c *youtube.Client, st *repo.Store, items []Item, log Logf) ImageResult {
	var res ImageResult
	for _, it := range items {
		if ctx.Err() != nil {
			break
		}
		asset := repo.Asset{Key: it.Key, Type: it.Type, Source: it.Source, Path: it.Path}

		if st.Exists(it.Path) {
			asset.Status = repo.StatusLocal
			res.Reused++
			res.Assets = append(res.Assets, asset)
			continue
		}

		data, err := fetch(ctx, c.HTTP(), it.Source)
		if err != nil {
			logf(log, "image %s: %v", it.Key, err)
			asset.Status = repo.StatusUnavailable
			asset.Path = ""
			res.Failed++
			res.Assets = append(res.Assets, asset)
			continue
		}
		if err := st.WriteMedia(it.Path, data); err != nil {
			logf(log, "write image %s: %v", it.Key, err)
			asset.Status = repo.StatusUnavailable
			asset.Path = ""
			res.Failed++
			res.Assets = append(res.Assets, asset)
			continue
		}
		asset.Status = repo.StatusLocal
		res.Downloaded++
		res.Assets = append(res.Assets, asset)
	}
	return res
}

// fetch GETs a URL and returns its body, treating a non-200 as an error so the
// caller records the asset unavailable rather than writing an error page to disk.
func fetch(ctx context.Context, hc *http.Client, url string) ([]byte, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return nil, err
	}
	resp, err := hc.Do(req)
	if err != nil {
		return nil, err
	}
	defer func() { _ = resp.Body.Close() }()
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("http %d", resp.StatusCode)
	}
	return io.ReadAll(resp.Body)
}

func logf(log Logf, format string, args ...any) {
	if log != nil {
		log(format, args...)
	}
}
