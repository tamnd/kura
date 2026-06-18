package tool

import (
	"context"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
)

func TestLocateEmptyIsNoTool(t *testing.T) {
	y, err := Locate("", "")
	if err != nil {
		t.Fatalf("Locate(\"\") error: %v", err)
	}
	if y != nil {
		t.Fatalf("Locate(\"\") = %v, want nil (no tool requested)", y)
	}
}

func TestLocateUnknownName(t *testing.T) {
	_, err := Locate("youtube-dl", "")
	if err == nil {
		t.Fatal("Locate(\"youtube-dl\") = nil error, want unsupported-tool error")
	}
	if !strings.Contains(err.Error(), "yt-dlp") {
		t.Fatalf("error %q should name yt-dlp", err.Error())
	}
}

func TestLocateUsesEnvOverride(t *testing.T) {
	t.Setenv("YTB_YT_DLP_BIN", "/opt/custom/yt-dlp")
	y, err := Locate("yt-dlp", "/usr/bin/ffmpeg")
	if err != nil {
		t.Fatalf("Locate error: %v", err)
	}
	if y.Bin != "/opt/custom/yt-dlp" {
		t.Fatalf("Bin = %q, want the env override", y.Bin)
	}
	if y.FFmpeg != "/usr/bin/ffmpeg" {
		t.Fatalf("FFmpeg = %q, want the passed path", y.FFmpeg)
	}
}

func TestLocateNotFoundNamesTool(t *testing.T) {
	t.Setenv("YTB_YT_DLP_BIN", "")
	t.Setenv("PATH", t.TempDir()) // nothing on PATH
	_, err := Locate("yt-dlp", "")
	if err == nil {
		t.Fatal("Locate(\"yt-dlp\") with empty PATH = nil error, want not-found error")
	}
	// The CLI classifies "yt-dlp" in the message as the needs-tool exit code.
	if !strings.Contains(err.Error(), "yt-dlp") {
		t.Fatalf("not-found error %q should name yt-dlp", err.Error())
	}
}

// stubYtDlp writes a tiny executable that mimics the slice of yt-dlp's CLI kura
// uses: it reads the -o template, substitutes the extension, and writes one file.
func stubYtDlp(t *testing.T, body string) string {
	t.Helper()
	if runtime.GOOS == "windows" {
		t.Skip("stub uses a POSIX shell script")
	}
	dir := t.TempDir()
	bin := filepath.Join(dir, "yt-dlp")
	if err := os.WriteFile(bin, []byte(body), 0o755); err != nil {
		t.Fatalf("write stub: %v", err)
	}
	return bin
}

func TestDownloadMovesSingleFileToDst(t *testing.T) {
	// The stub finds the -o argument, turns media.%(ext)s into media.mp4, and
	// writes the bytes kura should end up with at dst.
	stub := stubYtDlp(t, `#!/bin/sh
out=""
while [ $# -gt 0 ]; do
  if [ "$1" = "-o" ]; then out="$2"; shift; fi
  shift
done
target=$(printf '%s' "$out" | sed 's/%(ext)s/mp4/')
printf 'video-bytes' > "$target"
`)
	y := &YtDlp{Bin: stub}
	dst := filepath.Join(t.TempDir(), "nested", "video.mp4")
	if err := y.Download(context.Background(), "https://youtu.be/abc", "b", dst); err != nil {
		t.Fatalf("Download: %v", err)
	}
	got, err := os.ReadFile(dst)
	if err != nil {
		t.Fatalf("read dst: %v", err)
	}
	if string(got) != "video-bytes" {
		t.Fatalf("dst content = %q, want %q", got, "video-bytes")
	}
}

func TestDownloadSurfacesToolFailure(t *testing.T) {
	stub := stubYtDlp(t, `#!/bin/sh
echo 'ERROR: [youtube] abc: Video unavailable' 1>&2
exit 1
`)
	y := &YtDlp{Bin: stub}
	dst := filepath.Join(t.TempDir(), "video.mp4")
	err := y.Download(context.Background(), "https://youtu.be/abc", "b", dst)
	if err == nil {
		t.Fatal("Download with a failing stub = nil error")
	}
	if !strings.Contains(err.Error(), "yt-dlp") {
		t.Fatalf("error %q should name yt-dlp for exit-code classification", err.Error())
	}
	if !strings.Contains(err.Error(), "Video unavailable") {
		t.Fatalf("error %q should carry the stderr tail", err.Error())
	}
}

func TestSubtitlesReadsVTT(t *testing.T) {
	stub := stubYtDlp(t, `#!/bin/sh
out=""
while [ $# -gt 0 ]; do
  if [ "$1" = "-o" ]; then out="$2"; shift; fi
  shift
done
target=$(printf '%s' "$out" | sed 's/%(ext)s/en.vtt/')
printf 'WEBVTT\n\nhello\n' > "$target"
`)
	y := &YtDlp{Bin: stub}
	vtt, err := y.Subtitles(context.Background(), "https://youtu.be/abc", "en")
	if err != nil {
		t.Fatalf("Subtitles: %v", err)
	}
	if !strings.Contains(string(vtt), "WEBVTT") {
		t.Fatalf("vtt = %q, want a WEBVTT body", vtt)
	}
}
