package archive

import (
	"context"
	"errors"
	"fmt"
	"sort"
	"strings"
	"time"

	"github.com/tamnd/kura/media"
	"github.com/tamnd/kura/repo"
	"github.com/tamnd/kura/tool"
	"github.com/tamnd/ytb-cli/youtube"
)

// Run captures a target into a repository under opts.Out and returns a summary.
// It is the one entry the CLI's archive/add commands call. The pipeline is:
// fetch the channel record (when the target has one), stream the video spine and
// capture each video's record and sidecars into the store (writing each as it
// arrives so an interrupted run keeps what it got), localise images and (at media
// or audio depth) streams, render the requested views, and write the manifest.
// Records always persist as JSON; media and views are derived and regenerable
// with `kura render`.
func Run(ctx context.Context, c *youtube.Client, t Target, opts Options, log Logf) (*Result, error) {
	root := t.Root(opts.Out)
	res := &Result{Root: root, Target: t}

	if opts.DryRun {
		res.DryRun = true
		say(log, "dry run: would capture %s into %s", t.Display, root)
		return res, nil
	}

	st, err := repo.Open(root)
	if err != nil {
		return res, err
	}

	mf, existed, err := repo.LoadManifest(root)
	if err != nil {
		return res, err
	}
	if !existed || mf == nil {
		mf = repo.NewManifest(t.TargetRef(), opts.Version)
	}
	mf.KuraVersion = opts.Version

	// Resume: on an existing, completely-captured channel or playlist, page only
	// the uploads newer than the newest already held (the engine streams
	// newest-first, so a known id is the incremental boundary). An interrupted
	// backfill, a disabled --resume, or an explicit --since-id re-walks, with the
	// on-disk records keeping the already-captured prefix cheap.
	if opts.Resume && !opts.Force && opts.SinceID == "" && (t.Kind == KindChannel || t.Kind == KindPlaylist) {
		if prev, ok, _ := repo.LoadState(root); ok && prev != nil && prev.Complete && prev.NewestID != "" {
			opts.SinceID = prev.NewestID
			say(log, "resume: incremental update, paging newer than %s", prev.NewestID)
		}
	}

	// Resolve the optional external downloader once. A bad --tool name or a tool
	// the system lacks is a setup error surfaced before any fetching begins.
	dl, err := tool.Locate(opts.Tool, opts.FFmpeg)
	if err != nil {
		return res, err
	}

	cp := &capturer{st: st, client: c, opts: opts, log: log, mf: mf, seen: map[string]bool{}, tool: dl}

	// Capture the channel record up front so the header and avatar are available
	// to every page, and so a channel spine has its identity.
	channel := cp.captureChannel(ctx, t)
	res.Channel = channel

	err = cp.captureSpine(ctx, t, res)
	// The capture is complete when the spine streamed to its natural end or reached
	// its incremental boundary; a budget stop, a cancel, or an error leaves it
	// partial, so a later resume re-walks to finish.
	cp.complete = (err == nil && ctx.Err() == nil) || cp.reachedSinceID
	// A budget stop or a context cancel is not a failure: keep what was written.
	if err != nil && !errors.Is(err, youtube.ErrStop) && ctx.Err() == nil {
		// Persist the partial archive, then surface the typed error so the CLI
		// maps it to the right exit code.
		_ = cp.finish(ctx, t, res, nil)
		return res, err
	}

	// Re-read the full record set (new plus previously held) so media and render
	// operate on the merged archive (spec §11).
	all, err := st.LoadVideos()
	if err != nil {
		return res, err
	}
	res.Videos = len(all)
	setRange(res, all)

	if err := cp.finish(ctx, t, res, all); err != nil {
		return res, err
	}
	if ctx.Err() != nil {
		return res, ctx.Err()
	}
	return res, nil
}

// finish runs the media, render, and manifest stages over the merged record set.
// When all is nil it re-loads the records itself (the partial-error path).
func (c *capturer) finish(ctx context.Context, t Target, res *Result, all []*youtube.Video) error {
	if all == nil {
		var err error
		if all, err = c.st.LoadVideos(); err != nil {
			return err
		}
		res.Videos = len(all)
		setRange(res, all)
	}

	assets := c.mf.MediaIndex

	// Localise images: thumbnails, the channel avatar and banner, and comment
	// author avatars (so a captured thread renders fully offline).
	items := media.PlanImages(res.Channel, all)
	items = append(items, c.commentAvatarItems(all)...)
	if len(items) > 0 {
		say(c.log, "images: %d planned", len(items))
		ir := media.DownloadImages(ctx, c.client, c.st, items, func(f string, a ...any) { say(c.log, f, a...) })
		res.MediaOK += ir.Downloaded + ir.Reused
		res.MediaFail += ir.Failed
		assets = mergeAssets(assets, ir.Assets)
	}

	// Localise streams at media or audio depth, skipping videos that already have
	// a local stream so `add --depth media` upgrades a meta repo in place.
	if c.opts.wantStreams() {
		have := localStreamIDs(assets, c.opts.Depth)
		var streamAssets []repo.Asset
		for _, v := range all {
			if ctx.Err() != nil {
				break
			}
			if have[v.VideoID] {
				continue
			}
			sr := media.FetchStream(ctx, c.client, c.st, v, c.streamOptions(), func(f string, a ...any) { say(c.log, f, a...) })
			switch {
			case sr.Reused, sr.Downloaded:
				res.StreamOK++
			case sr.Asset.Status == repo.StatusStreamOnly:
				res.StreamOnly++
			default:
				res.StreamFail++
			}
			for _, g := range sr.Gaps {
				c.mf.AddGap(v.VideoID, g.What, g.Reason)
			}
			streamAssets = append(streamAssets, sr.Asset)
		}
		assets = mergeAssets(assets, streamAssets)
	}

	res.Transcripts = c.countTranscripts(all)
	res.Comments = c.opts.wantComments() || c.mf.Comments

	if err := renderAll(c.st, all, res.Channel, assets, t, c.opts); err != nil {
		return err
	}

	// Manifest: counts, range, media index, and a capture entry (the only
	// wall-clock value, KR5).
	c.mf.Depth = string(c.opts.Depth)
	c.mf.Videos = len(all)
	c.mf.Transcripts = res.Transcripts
	c.mf.Comments = res.Comments
	c.mf.Media = mediaCounts(assets)
	c.mf.MediaIndex = assets
	c.mf.Range = repoRange(all)
	if res.Channel != nil && res.Channel.ChannelID != "" {
		c.mf.Target.ChannelID = res.Channel.ChannelID
	}
	c.mf.AddCapture(c.opts.stamp().Format(time.RFC3339), res.Added, string(c.opts.Depth))
	res.Gaps = len(c.mf.Gaps)
	for _, g := range c.mf.Gaps {
		if g.What == "comments" || g.What == "transcript" {
			res.Gated++
		}
	}
	if res.Gated > 0 {
		res.Note(fmt.Sprintf("%d surface(s) IP-gated (comments/transcript hidden); a yt-dlp fallback may reach them", res.Gated))
	}
	if err := c.mf.Save(c.st.Dir()); err != nil {
		return err
	}
	c.saveState(t, all)
	return nil
}

// saveState writes the resume cursor: the captured id/time range plus whether the
// spine was exhausted this run. It is best-effort, since the records on disk are
// the source of truth and a missing cursor only costs a re-walk.
func (c *capturer) saveState(t Target, all []*youtube.Video) {
	newestID, oldestID, newestAt, oldestAt := rangeIDs(all)
	state := &repo.State{
		Target:    t.TargetRef(),
		Depth:     string(c.opts.Depth),
		Videos:    len(all),
		NewestID:  newestID,
		OldestID:  oldestID,
		NewestAt:  newestAt,
		OldestAt:  oldestAt,
		Complete:  c.complete,
		UpdatedAt: c.opts.stamp().Format(time.RFC3339),
	}
	_ = state.Save(c.st.Dir())
}

// rangeIDs returns the newest and oldest video ids and their published times by
// publish order, so the resume cursor can name the incremental boundary.
func rangeIDs(all []*youtube.Video) (newestID, oldestID, newestAt, oldestAt string) {
	var newest, oldest *youtube.Video
	for _, v := range all {
		if v.PublishedAt.IsZero() {
			continue
		}
		if newest == nil || v.PublishedAt.After(newest.PublishedAt) {
			newest = v
		}
		if oldest == nil || v.PublishedAt.Before(oldest.PublishedAt) {
			oldest = v
		}
	}
	if newest != nil {
		newestID, newestAt = newest.VideoID, newest.PublishedAt.UTC().Format(time.RFC3339)
	}
	if oldest != nil {
		oldestID, oldestAt = oldest.VideoID, oldest.PublishedAt.UTC().Format(time.RFC3339)
	}
	return newestID, oldestID, newestAt, oldestAt
}

// localStreamIDs returns the set of video ids that already have a local stream of
// the right kind on disk, so a re-run does not re-select or re-download them.
func localStreamIDs(assets []repo.Asset, depth media.Depth) map[string]bool {
	prefix := "video:"
	if depth == media.DepthAudio {
		prefix = "audio:"
	}
	out := map[string]bool{}
	for _, a := range assets {
		if a.Status != repo.StatusLocal || a.Path == "" {
			continue
		}
		if id, ok := strings.CutPrefix(a.Key, prefix); ok {
			out[id] = true
		}
	}
	return out
}

// commentAvatarItems plans a download for each distinct comment author avatar in
// the captured comment sidecars.
func (c *capturer) commentAvatarItems(all []*youtube.Video) []media.Item {
	var out []media.Item
	seen := map[string]bool{}
	for _, v := range all {
		comments, err := c.st.LoadComments(v.VideoID)
		if err != nil {
			continue
		}
		for _, cm := range comments {
			if cm.AuthorProfileImage == "" || seen[cm.AuthorProfileImage] {
				continue
			}
			seen[cm.AuthorProfileImage] = true
			if it, ok := media.CommentAvatarItem(cm.AuthorDisplayName, cm.AuthorProfileImage); ok {
				out = append(out, it)
			}
		}
	}
	return out
}

// setRange records the captured published-time span on the result.
func setRange(res *Result, all []*youtube.Video) {
	r := repoRange(all)
	if !r.Oldest.IsZero() {
		res.Oldest = r.Oldest.UTC().Format(time.RFC3339)
	}
	if !r.Newest.IsZero() {
		res.Newest = r.Newest.UTC().Format(time.RFC3339)
	}
}

func repoRange(all []*youtube.Video) repo.Range {
	var r repo.Range
	for _, v := range all {
		if v.PublishedAt.IsZero() {
			continue
		}
		if r.Oldest.IsZero() || v.PublishedAt.Before(r.Oldest) {
			r.Oldest = v.PublishedAt
		}
		if r.Newest.IsZero() || v.PublishedAt.After(r.Newest) {
			r.Newest = v.PublishedAt
		}
	}
	return r
}

// mergeAssets unions previously held assets with this run's, keyed by asset key
// (and source for images), preferring the fresher record. The result is sorted
// for a stable manifest (KR5).
func mergeAssets(old, fresh []repo.Asset) []repo.Asset {
	by := map[string]repo.Asset{}
	keyOf := func(a repo.Asset) string { return a.Key + "\x00" + a.Source }
	for _, a := range old {
		by[keyOf(a)] = a
	}
	for _, a := range fresh {
		by[keyOf(a)] = a
	}
	out := make([]repo.Asset, 0, len(by))
	for _, a := range by {
		out = append(out, a)
	}
	sort.Slice(out, func(i, j int) bool {
		if out[i].Key != out[j].Key {
			return out[i].Key < out[j].Key
		}
		return out[i].Source < out[j].Source
	})
	return out
}

// mediaCounts tallies the local media on disk by kind for the manifest summary.
func mediaCounts(assets []repo.Asset) repo.MediaCounts {
	var mc repo.MediaCounts
	for _, a := range assets {
		if a.Status != repo.StatusLocal {
			continue
		}
		switch a.Type {
		case "thumb", "avatar", "banner":
			mc.Thumbs++
		case "video":
			mc.Videos++
		case "audio":
			mc.Audio++
		}
	}
	return mc
}
