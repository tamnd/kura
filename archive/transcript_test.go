package archive

import "testing"

func TestVTTToText(t *testing.T) {
	vtt := "WEBVTT\n" +
		"Kind: captions\n" +
		"Language: en\n" +
		"\n" +
		"1\n" +
		"00:00:00.000 --> 00:00:02.000 align:start position:0%\n" +
		"<c.colorE5E5E5>Hello</c> there\n" +
		"\n" +
		"2\n" +
		"00:00:02.000 --> 00:00:04.000\n" +
		"Hello there\n" +
		"\n" +
		"3\n" +
		"00:00:04.000 --> 00:00:06.000\n" +
		"welcome to <00:00:05.000>the show\n"

	got := vttToText(vtt)
	want := "Hello there\nwelcome to the show\n"
	if got != want {
		t.Fatalf("vttToText\n got: %q\nwant: %q", got, want)
	}
}

func TestVTTToTextEmpty(t *testing.T) {
	if got := vttToText("WEBVTT\n\n"); got != "" {
		t.Fatalf("vttToText of header-only = %q, want empty", got)
	}
}
