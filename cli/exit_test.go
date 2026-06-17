package cli

import (
	"context"
	"errors"
	"testing"

	"github.com/tamnd/kura/archive"
	"github.com/tamnd/ytb-cli/youtube"
)

func TestClassifyMessage(t *testing.T) {
	cases := map[string]int{
		"HTTP 429 Too Many Requests":  CodeBlocked,
		"request blocked by captcha":  CodeBlocked,
		"HTTP 404 not found":          CodeNotFound,
		"this video is private":       CodeNotFound,
		"channel terminated":          CodeNotFound,
		"ffmpeg is required to merge": CodeNotFound,
		"yt-dlp not found on PATH":    CodeNeedsTool,
		"some other parsing problem":  CodeUsage,
	}
	for msg, want := range cases {
		if got := classifyMessage(errors.New(msg)); got != want {
			t.Errorf("classifyMessage(%q) = %d, want %d", msg, got, want)
		}
	}
}

func TestCodeForSentinels(t *testing.T) {
	ctx := context.Background()
	if got := codeFor(ctx, youtube.ErrFFmpegMissing); got != CodeNotFound {
		t.Errorf("ffmpeg sentinel = %d, want %d", got, CodeNotFound)
	}
	if got := codeFor(ctx, youtube.ErrCommentsRestricted); got != CodeGated {
		t.Errorf("comments sentinel = %d, want %d", got, CodeGated)
	}
	canceled, cancel := context.WithCancel(context.Background())
	cancel()
	if got := codeFor(canceled, context.Canceled); got != CodeInterrupt {
		t.Errorf("canceled = %d, want %d", got, CodeInterrupt)
	}
}

func TestCodeForRun(t *testing.T) {
	ctx := context.Background()
	cases := []struct {
		name string
		res  *archive.Result
		err  error
		want int
	}{
		{"clean", &archive.Result{Videos: 5}, nil, CodeOK},
		{"no results", &archive.Result{Videos: 0}, nil, CodeNoResults},
		{"gated", &archive.Result{Videos: 5, Gated: 1}, nil, CodeGated},
		{"partial stream", &archive.Result{Videos: 5, StreamFail: 2}, nil, CodePartial},
		{"dry run", &archive.Result{DryRun: true}, nil, CodeOK},
		{"hard error but wrote repo", &archive.Result{Videos: 3}, errors.New("parse glitch"), CodePartial},
		{"hard error no repo", &archive.Result{Videos: 0}, errors.New("HTTP 404 not found"), CodeNotFound},
	}
	for _, c := range cases {
		if got := codeForRun(ctx, c.res, c.err); got != c.want {
			t.Errorf("%s: codeForRun = %d, want %d", c.name, got, c.want)
		}
	}
}

func TestWorseCode(t *testing.T) {
	if worseCode(CodeOK, CodeGated) != CodeGated {
		t.Error("OK should yield to a worse code")
	}
	if worseCode(CodeBlocked, CodeOK) != CodeBlocked {
		t.Error("a worse code should survive an OK")
	}
	if worseCode(CodePartial, CodeBlocked) != CodeBlocked {
		t.Error("the more severe code should win")
	}
}

func TestParseViews(t *testing.T) {
	v, err := parseViews("html,md")
	if err != nil || len(v) != 2 {
		t.Fatalf("html,md => %v err=%v", v, err)
	}
	if _, err := parseViews("pdf"); err == nil {
		t.Error("unknown view should error")
	}
	if _, err := parseViews(""); err == nil {
		t.Error("empty view set should error")
	}
}
