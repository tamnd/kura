package cli

// Build metadata, overridden at release time via -ldflags
// -X github.com/tamnd/kura/cli.Version=... (and Commit/Date). The defaults are
// what a plain `go build` or `go install` reports.
var (
	Version = "dev"
	Commit  = "none"
	Date    = "unknown"
)
