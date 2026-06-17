package cli

import (
	"fmt"
	"net"
	"net/http"

	"github.com/spf13/cobra"
	"github.com/tamnd/kura/repo"
)

// newServeCmd builds `kura serve <repo>`: a thin static file server over the
// repository so a user can browse the inert archive, and at media depth play the
// downloaded streams, in a browser (spec §12). It serves files as-is; the archive
// is already self-contained, so this is only a convenience over opening index.html
// directly.
func newServeCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "serve <repo>",
		Short: "Serve a repository over http://localhost for preview and playback",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			root := args[0]
			if _, ok, err := repo.LoadManifest(root); err != nil {
				return err
			} else if !ok {
				return fmt.Errorf("%s is not a kura repository (no manifest.json)", root)
			}
			addr, _ := cmd.Flags().GetString("addr")

			ln, err := net.Listen("tcp", addr)
			if err != nil {
				return err
			}
			srv := &http.Server{Handler: http.FileServer(http.Dir(root))}
			_, _ = fmt.Fprintf(cmd.OutOrStdout(), "serving %s at http://%s/ (Ctrl-C to stop)\n", root, ln.Addr())

			go func() {
				<-cmd.Context().Done()
				_ = srv.Close()
			}()
			if err := srv.Serve(ln); err != nil && err != http.ErrServerClosed {
				return err
			}
			return nil
		},
	}
	cmd.Flags().String("addr", "127.0.0.1:8080", "address to listen on")
	return cmd
}
