// Command kura builds offline, browsable archives of YouTube content.
// It is a thin entry point: it wires a signal-aware context so Ctrl-C cancels an
// in-flight capture cleanly, hands off to the cli package, and exits with the code
// that package maps from the outcome (spec §12).
package main

import (
	"context"
	"os"
	"os/signal"
	"syscall"

	"github.com/tamnd/kura/cli"
)

func main() {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	os.Exit(cli.Execute(ctx))
}
