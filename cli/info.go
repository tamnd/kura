package cli

import (
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strings"

	"github.com/spf13/cobra"
	"github.com/tamnd/kura/repo"
)

// newInfoCmd builds `kura info <repo>`: a manifest summary — what the repo
// archives, how deep, how many records, media and transcripts, the date range,
// the capture history, the recorded gaps, and the on-disk size.
func newInfoCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "info <repo>",
		Short: "Summarise a repository: counts, depth, range, gaps, size",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			root := args[0]
			mf, ok, err := repo.LoadManifest(root)
			if err != nil {
				return err
			}
			if !ok || mf == nil {
				return fmt.Errorf("%s is not a kura repository (no manifest.json)", root)
			}
			var b strings.Builder
			fmt.Fprintf(&b, "repository:  %s\n", root)
			fmt.Fprintf(&b, "service:     %s\n", mf.Service)
			fmt.Fprintf(&b, "target:      %s %s", mf.Target.Kind, mf.Target.Ref)
			if mf.Target.ChannelID != "" {
				fmt.Fprintf(&b, " (channel %s)", mf.Target.ChannelID)
			}
			fmt.Fprintln(&b)
			fmt.Fprintf(&b, "depth:       %s\n", mf.Depth)
			fmt.Fprintf(&b, "videos:      %d\n", mf.Videos)
			fmt.Fprintf(&b, "transcripts: %d\n", mf.Transcripts)
			fmt.Fprintf(&b, "media:       %d thumbs, %d videos, %d audio\n",
				mf.Media.Thumbs, mf.Media.Videos, mf.Media.Audio)
			if !mf.Range.Oldest.IsZero() {
				fmt.Fprintf(&b, "range:       %s … %s\n",
					mf.Range.Oldest.UTC().Format("2006-01-02"),
					mf.Range.Newest.UTC().Format("2006-01-02"))
			}
			fmt.Fprintf(&b, "captures:    %d\n", len(mf.Captures))
			for _, c := range mf.Captures {
				fmt.Fprintf(&b, "  %s  +%d at depth %s\n", c.At, c.Added, c.Depth)
			}
			if len(mf.Gaps) > 0 {
				fmt.Fprintf(&b, "gaps:        %d\n", len(mf.Gaps))
				for _, g := range mf.Gaps {
					if g.VideoID != "" {
						fmt.Fprintf(&b, "  %s %s: %s\n", g.VideoID, g.What, g.Reason)
					} else {
						fmt.Fprintf(&b, "  %s: %s\n", g.What, g.Reason)
					}
				}
			}
			if size, err := dirSize(root); err == nil {
				fmt.Fprintf(&b, "size:        %s\n", humanBytes(size))
			}
			_, err = fmt.Fprint(cmd.OutOrStdout(), b.String())
			return err
		},
	}
}

// dirSize sums the size of every regular file under root.
func dirSize(root string) (int64, error) {
	var total int64
	err := filepath.WalkDir(root, func(_ string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.IsDir() {
			return nil
		}
		info, err := d.Info()
		if err != nil {
			return err
		}
		total += info.Size()
		return nil
	})
	if os.IsNotExist(err) {
		return 0, nil
	}
	return total, err
}

// humanBytes renders a byte count as a compact human-readable size.
func humanBytes(n int64) string {
	const unit = 1024
	if n < unit {
		return fmt.Sprintf("%d B", n)
	}
	div, exp := int64(unit), 0
	for x := n / unit; x >= unit; x /= unit {
		div *= unit
		exp++
	}
	return fmt.Sprintf("%.1f %cB", float64(n)/float64(div), "KMGTPE"[exp])
}
