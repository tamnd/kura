package media

import (
	"context"
	"os"
	"path/filepath"

	"github.com/tamnd/kura/repo"
	"github.com/tamnd/ytb-cli/youtube"
)

// defaultWorkers is the per-stream range concurrency. The engine walks a stream
// in 9 MiB chunks; a handful of workers saturates a connection without tripping
// googlevideo's throttling.
const defaultWorkers = 4

// Default format specs per depth. media wants the best video plus the best audio
// (merged), falling back to the best progressive stream; audio wants the best
// audio-only track.
const (
	defaultVideoFormat = "bv*+ba/b"
	defaultAudioFormat = "ba/ba*"
	progressiveFormat  = "b/bv*"
)

// Gap is a stream a capture could not localise, to be recorded in the manifest.
type Gap struct {
	What   string
	Reason string
}

// StreamResult is the outcome of localising one video's stream.
type StreamResult struct {
	Asset      repo.Asset
	Gaps       []Gap
	Downloaded bool
	Reused     bool
}

// StreamOptions configures one FetchStream call.
type StreamOptions struct {
	Depth      Depth
	Format     string // explicit -f spec, or "" for the depth default
	FFmpeg     string // explicit ffmpeg path, or "" to auto-locate
	Workers    int
	OnProgress func(done, total int64)
}

// FetchStream localises one video's stream through the engine's pure-Go download
// path: resolve the manifest, select a rendition, fetch it in ranged chunks, and
// merge the video+audio pair with ffmpeg when needed. A live stream, a missing
// rendition, or a needed-but-absent ffmpeg is recorded as a gap and asset status
// rather than aborting the capture (KR4). The returned asset carries the
// "video:<id>" or "audio:<id>" key the renderers use to point the player at the
// local file.
func FetchStream(ctx context.Context, c *youtube.Client, st *repo.Store, v *youtube.Video, opts StreamOptions, log Logf) StreamResult {
	id := v.VideoID
	audioOnly := opts.Depth == DepthAudio
	key, mtype := "video:"+id, "video"
	if audioOnly {
		key, mtype = "audio:"+id, "audio"
	}
	source := v.URL
	if source == "" {
		source = youtube.NormalizeVideoURL(id)
	}
	miss := func(status, what, reason string) StreamResult {
		logf(log, "stream %s: %s", id, reason)
		return StreamResult{
			Asset: repo.Asset{Key: key, Type: mtype, Source: source, Status: status},
			Gaps:  []Gap{{What: what, Reason: reason}},
		}
	}

	manifest, err := c.StreamManifest(ctx, id)
	if err != nil {
		return miss(repo.StatusUnavailable, "stream", err.Error())
	}
	if manifest.IsLive {
		return miss(repo.StatusStreamOnly, "stream", "live stream, not archivable as a file")
	}

	spec := opts.Format
	if spec == "" {
		if audioOnly {
			spec = defaultAudioFormat
		} else {
			spec = defaultVideoFormat
		}
	}
	sel, err := youtube.SelectFormat(manifest.Streams, spec)
	if err != nil {
		return miss(repo.StatusStreamOnly, "stream", "no matching format: "+err.Error())
	}

	// A merge needs ffmpeg; without it, fall back to a progressive muxed stream
	// so the archive still gets a playable file rather than nothing.
	ffmpeg := ""
	if sel.NeedsMerge() && !audioOnly {
		ffmpeg = youtube.FFmpeg(opts.FFmpeg)
		if ffmpeg == "" {
			if prog, perr := youtube.SelectFormat(manifest.Streams, progressiveFormat); perr == nil && prog.Video != nil && prog.Audio == nil {
				sel = prog
				logf(log, "stream %s: ffmpeg missing, using progressive rendition", id)
			} else {
				return miss(repo.StatusStreamOnly, "stream", "adaptive streams need ffmpeg to merge (install it or pass --ffmpeg-bin)")
			}
		}
	}

	token := streamFormatToken(sel)
	workers := opts.Workers
	if workers < 1 {
		workers = defaultWorkers
	}

	var dst string
	if audioOnly {
		a := sel.Audio
		if a == nil {
			a = sel.Video // a muxed fallback still carries audio
		}
		if a == nil {
			return miss(repo.StatusStreamOnly, "stream", "no audio rendition")
		}
		dst = repo.AudioMediaPath(id, token, a.Ext())
	} else {
		ext := "mp4"
		if !sel.NeedsMerge() && sel.Video != nil {
			ext = sel.Video.Ext()
		}
		dst = repo.VideoMediaPath(id, token, ext)
	}

	asset := repo.Asset{Key: key, Type: mtype, Source: source, Path: dst, Status: repo.StatusLocal}
	if st.Exists(dst) {
		return StreamResult{Asset: asset, Reused: true}
	}

	if sel.NeedsMerge() && !audioOnly {
		if err := fetchMerged(ctx, c, st, manifest, sel, dst, ffmpeg, workers, opts.OnProgress); err != nil {
			return miss(repo.StatusUnavailable, "stream", err.Error())
		}
		return StreamResult{Asset: asset, Downloaded: true}
	}

	one := sel.Video
	if audioOnly && sel.Audio != nil {
		one = sel.Audio
	}
	if one == nil {
		return miss(repo.StatusStreamOnly, "stream", "empty selection")
	}
	if err := downloadStreamTo(ctx, c, st, manifest, one, dst, workers, opts.OnProgress); err != nil {
		return miss(repo.StatusUnavailable, "stream", err.Error())
	}
	return StreamResult{Asset: asset, Downloaded: true}
}

// fetchMerged downloads the adaptive video and audio tracks to temporary files,
// muxes them into dst with ffmpeg, then removes the temporaries.
func fetchMerged(ctx context.Context, c *youtube.Client, st *repo.Store, m *youtube.StreamManifest, sel youtube.Selection, dst, ffmpeg string, workers int, onProgress func(done, total int64)) error {
	abs := st.Abs(dst)
	if err := os.MkdirAll(filepath.Dir(abs), 0o755); err != nil {
		return err
	}
	vTmp := abs + ".part-v"
	aTmp := abs + ".part-a"
	defer func() { _ = os.Remove(vTmp); _ = os.Remove(aTmp) }()

	if err := downloadStreamTo(ctx, c, st, m, sel.Video, dst+".part-v", workers, onProgress); err != nil {
		return err
	}
	if err := downloadStreamTo(ctx, c, st, m, sel.Audio, dst+".part-a", workers, onProgress); err != nil {
		return err
	}
	return youtube.MergeAV(ctx, ffmpeg, vTmp, aTmp, abs)
}

// downloadStreamTo resolves a single stream's URL and fetches it to a
// repository-relative destination, creating parent directories first.
func downloadStreamTo(ctx context.Context, c *youtube.Client, st *repo.Store, m *youtube.StreamManifest, s *youtube.Stream, dst string, workers int, onProgress func(done, total int64)) error {
	abs := st.Abs(dst)
	if err := os.MkdirAll(filepath.Dir(abs), 0o755); err != nil {
		return err
	}
	rawURL, err := c.ResolveStreamURL(ctx, m, s)
	if err != nil {
		return err
	}
	var cb func(youtube.DownloadProgress)
	if onProgress != nil {
		cb = func(p youtube.DownloadProgress) { onProgress(p.Downloaded, p.Total) }
	}
	return c.DownloadToFile(ctx, rawURL, abs, s.ContentLength, workers, cb)
}
