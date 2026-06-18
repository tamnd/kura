// Package tool delegates the hard cases to an external command-line downloader.
// kura's engine fetches everything itself for free, but a flagged egress IP can
// hide a transcript or refuse a stream that yt-dlp, with its own client pool and
// signature handling, still reaches. The delegation is opt-in through --tool and
// best-effort: a missing tool or a tool failure is surfaced honestly through the
// manifest and the exit code, never papered over.
package tool

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

// YtDlp is a resolved yt-dlp binary plus the optional ffmpeg it should use for a
// merge. The zero value is not usable; build one with Locate.
type YtDlp struct {
	Bin    string // resolved binary path
	FFmpeg string // ffmpeg location to pass through, or "" for yt-dlp's own lookup
}

// Locate resolves the downloader named by the --tool flag. An empty name means no
// tool was requested and returns (nil, nil), so callers can treat "no tool" and
// "tool configured" uniformly. Only yt-dlp is supported today; an unknown name is
// an error. The binary is found at $YTB_YT_DLP_BIN, else the name as an explicit
// path, else on PATH. A not-found result names yt-dlp so the CLI maps it to the
// needs-tool exit code.
func Locate(name, ffmpeg string) (*YtDlp, error) {
	name = strings.TrimSpace(name)
	if name == "" {
		return nil, nil
	}
	if name != "yt-dlp" {
		return nil, fmt.Errorf("unknown --tool %q (only yt-dlp is supported)", name)
	}
	bin, err := resolveBin(name)
	if err != nil {
		return nil, err
	}
	return &YtDlp{Bin: bin, FFmpeg: ffmpeg}, nil
}

// resolveBin finds the yt-dlp binary: the YTB_YT_DLP_BIN override (shared with the
// ytb-cli toolchain), else the name if it is an explicit path, else PATH.
func resolveBin(name string) (string, error) {
	if env := strings.TrimSpace(os.Getenv("YTB_YT_DLP_BIN")); env != "" {
		return env, nil
	}
	if strings.ContainsRune(name, os.PathSeparator) {
		return name, nil
	}
	bin, err := exec.LookPath(name)
	if err != nil {
		return "", fmt.Errorf("yt-dlp not found on PATH; install it or set YTB_YT_DLP_BIN")
	}
	return bin, nil
}

// Download fetches videoURL into dst with the given yt-dlp format selector. yt-dlp
// decides the container, so the bytes land in a temporary directory under a fixed
// stem and the single produced file is moved to dst, keeping kura's deterministic
// on-disk name. dst's parent is created first.
func (y *YtDlp) Download(ctx context.Context, videoURL, format, dst string) error {
	if err := os.MkdirAll(filepath.Dir(dst), 0o755); err != nil {
		return err
	}
	tmp, err := os.MkdirTemp("", "kura-ytdlp-")
	if err != nil {
		return err
	}
	defer func() { _ = os.RemoveAll(tmp) }()

	out := filepath.Join(tmp, "media.%(ext)s")
	args := []string{"--no-playlist", "--no-progress", "--no-part", "-o", out}
	if format != "" {
		args = append(args, "-f", format)
	}
	if y.FFmpeg != "" {
		args = append(args, "--ffmpeg-location", y.FFmpeg)
	}
	args = append(args, videoURL)

	if err := y.run(ctx, args); err != nil {
		return err
	}
	produced, err := singleFile(tmp)
	if err != nil {
		return err
	}
	return os.Rename(produced, dst)
}

// Subtitles fetches the subtitle track for videoURL in lang (or the default track
// when lang is empty) as WebVTT, returning the .vtt bytes. It downloads no media.
// An empty lang lets yt-dlp pick; manual subtitles are preferred over auto-captions
// by trying them first.
func (y *YtDlp) Subtitles(ctx context.Context, videoURL, lang string) ([]byte, error) {
	tmp, err := os.MkdirTemp("", "kura-ytsub-")
	if err != nil {
		return nil, err
	}
	defer func() { _ = os.RemoveAll(tmp) }()

	langs := lang
	if langs == "" {
		langs = "en.*,en,.*"
	}
	out := filepath.Join(tmp, "sub.%(ext)s")
	args := []string{
		"--skip-download", "--no-playlist", "--no-progress",
		"--write-subs", "--write-auto-subs",
		"--sub-langs", langs, "--sub-format", "vtt",
		"-o", out, videoURL,
	}
	if err := y.run(ctx, args); err != nil {
		return nil, err
	}
	vtt, err := firstFileWithExt(tmp, ".vtt")
	if err != nil {
		return nil, err
	}
	return os.ReadFile(vtt)
}

// run executes yt-dlp, folding its stderr into the returned error so a failure is
// legible. The error text carries "yt-dlp" so the CLI classifies it as a tool
// problem rather than a missing target.
func (y *YtDlp) run(ctx context.Context, args []string) error {
	cmd := exec.CommandContext(ctx, y.Bin, args...)
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		msg := strings.TrimSpace(stderr.String())
		if msg == "" {
			msg = err.Error()
		}
		return fmt.Errorf("yt-dlp: %s", lastLine(msg))
	}
	return nil
}

// singleFile returns the one regular file in dir, erroring when there is not
// exactly one (yt-dlp produced nothing, or more than the expected output).
func singleFile(dir string) (string, error) {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return "", err
	}
	var found string
	for _, e := range entries {
		if e.IsDir() {
			continue
		}
		if found != "" {
			return "", fmt.Errorf("yt-dlp produced more than one file")
		}
		found = filepath.Join(dir, e.Name())
	}
	if found == "" {
		return "", fmt.Errorf("yt-dlp produced no output")
	}
	return found, nil
}

// firstFileWithExt returns the first file in dir whose name ends in ext.
func firstFileWithExt(dir, ext string) (string, error) {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return "", err
	}
	for _, e := range entries {
		if !e.IsDir() && strings.HasSuffix(e.Name(), ext) {
			return filepath.Join(dir, e.Name()), nil
		}
	}
	return "", fmt.Errorf("yt-dlp wrote no %s subtitle", ext)
}

// lastLine returns the last non-empty line of s, which for a yt-dlp failure is the
// ERROR line rather than the banner.
func lastLine(s string) string {
	lines := strings.Split(s, "\n")
	for i := len(lines) - 1; i >= 0; i-- {
		if t := strings.TrimSpace(lines[i]); t != "" {
			return t
		}
	}
	return s
}
