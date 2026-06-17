package archive

import "strings"

// Logf is the optional progress sink the CLI passes in. nil silences progress.
type Logf func(format string, args ...any)

// say emits a progress line when a sink is set.
func say(log Logf, format string, args ...any) {
	if log != nil {
		log(format, args...)
	}
}

// oneLine trims a string to a single short line for verbose per-record logging.
func oneLine(s string) string {
	s = strings.ReplaceAll(s, "\n", " ")
	r := []rune(s)
	if len(r) > 60 {
		return string(r[:60]) + "…"
	}
	return s
}
