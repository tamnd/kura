package media

import (
	"context"
	"os"
	"path/filepath"

	"github.com/tamnd/kura/repo"
	"github.com/tamnd/kura/tool"
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
	Quality    int    // cap the selected video height when > 0
	FFmpeg     string // explicit ffmpeg path, or "" to auto-locate
	Workers    int
	Tool       *tool.YtDlp // optional external downloader for the hard cases
	OnProgress func(done, total int64)
}

// spec is the format selector this capture should use: the explicit -f spec or
// the depth default, narrowed by the height cap (which never touches an audio
// capture, since audio has no height).
func (o StreamOptions) spec() string {
	s := o.Format
	if s == "" {
		if o.Depth == DepthAudio {
			s = defaultAudioFormat
		} else {
			s = defaultVideoFormat
		}
	}
	if o.Depth != DepthAudio {
		s = capHeight(s, o.Quality)
	}
	return s
}

// FetchStream localises one video's stream through the engine's pure-Go download
// path: resolve the manifest, select a rendition, fetch it in ranged chunks, and
// merge the video+audio pair with ffmpeg when needed. A live stream, a missing
// rendition, or a needed-but-absent ffmpeg is recorded as a gap and asset status
// rather than aborting the capture (KR4). The returned asset carries the
// "video:<id>" or "audio:<id>" key the renderers use to point the player at the
// local file.
func FetchStream(ctx context.Context, c *youtube.Client, st *repo.Store, v *youtube.Video, opts StreamOptions, log Logf) StreamResult {
	res := fetchStreamNative(ctx, c, st, v, opts, log)
	// A configured tool is a rescue path: when the native engine could not produce
	// a file (a deciphering failure, a missing rendition, an ffmpeg-less merge),
	// hand the video to yt-dlp. A reused or freshly downloaded file is left alone.
	if opts.Tool != nil && !res.Downloaded && !res.Reused {
		if tr, ok := fetchStreamTool(ctx, st, v, opts, log); ok {
			return tr
		}
	}
	return res
}

// fetchStreamNative localises a stream through the engine's pure-Go download path.
func fetchStreamNative(ctx context.Context, c *youtube.Client, st *repo.Store, v *youtube.Video, opts StreamOptions, log Logf) StreamResult {
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

	spec := opts.spec()
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
	if err := downloadAtomic(ctx, c, st, manifest, one, dst, workers, opts.OnProgress); err != nil {
		return miss(repo.StatusUnavailable, "stream", err.Error())
	}
	return StreamResult{Asset: asset, Downloaded: true}
}

// fetchStreamTool localises a stream by delegating to the configured external
// downloader (yt-dlp), for the videos the native path could not reach. It writes a
// deterministic file named with a "ytdlp" token so a re-run reuses it, and reports
// ok=false (leaving the native gap in place) when the tool also fails.
func fetchStreamTool(ctx context.Context, st *repo.Store, v *youtube.Video, opts StreamOptions, log Logf) (StreamResult, bool) {
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

	var dst string
	if audioOnly {
		dst = repo.AudioMediaPath(id, "ytdlp", "m4a")
	} else {
		dst = repo.VideoMediaPath(id, "ytdlp", "mp4")
	}
	asset := repo.Asset{Key: key, Type: mtype, Source: source, Path: dst, Status: repo.StatusLocal}
	if st.Exists(dst) {
		return StreamResult{Asset: asset, Reused: true}, true
	}

	logf(log, "stream %s: trying yt-dlp", id)
	if err := opts.Tool.Download(ctx, source, opts.spec(), st.Abs(dst)); err != nil {
		logf(log, "stream %s: yt-dlp failed: %s", id, err.Error())
		return StreamResult{}, false
	}
	return StreamResult{Asset: asset, Downloaded: true}, true
}

// fetchMerged downloads the adaptive video and audio tracks to temporary files,
// muxes them into a .part sibling of dst, then renames it into place so a killed
// merge never leaves a partial file that a re-run would mistake for complete. The
// temporaries are removed on the way out.
func fetchMerged(ctx context.Context, c *youtube.Client, st *repo.Store, m *youtube.StreamManifest, sel youtube.Selection, dst, ffmpeg string, workers int, onProgress func(done, total int64)) error {
	abs := st.Abs(dst)
	if err := os.MkdirAll(filepath.Dir(abs), 0o755); err != nil {
		return err
	}
	vTmp := abs + ".part-v"
	aTmp := abs + ".part-a"
	part := abs + ".part"
	defer func() { _ = os.Remove(vTmp); _ = os.Remove(aTmp); _ = os.Remove(part) }()

	if err := downloadStreamTo(ctx, c, st, m, sel.Video, dst+".part-v", workers, onProgress); err != nil {
		return err
	}
	if err := downloadStreamTo(ctx, c, st, m, sel.Audio, dst+".part-a", workers, onProgress); err != nil {
		return err
	}
	if err := youtube.MergeAV(ctx, ffmpeg, vTmp, aTmp, part); err != nil {
		return err
	}
	return os.Rename(part, abs)
}

// downloadAtomic fetches a single stream to a .part sibling of dst and renames it
// into place only on success. The engine's downloader truncates and seeks into
// its target, so a process killed mid-download leaves a partial file; writing to
// a .part and renaming means the final name appears only when the bytes are all
// there, and a re-run re-downloads the incomplete stream rather than reusing a
// truncated one.
func downloadAtomic(ctx context.Context, c *youtube.Client, st *repo.Store, m *youtube.StreamManifest, s *youtube.Stream, dst string, workers int, onProgress func(done, total int64)) error {
	part := dst + ".part"
	if err := downloadStreamTo(ctx, c, st, m, s, part, workers, onProgress); err != nil {
		_ = os.Remove(st.Abs(part))
		return err
	}
	return os.Rename(st.Abs(part), st.Abs(dst))
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
