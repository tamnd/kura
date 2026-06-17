package cli

import (
	"fmt"

	"github.com/spf13/cobra"
	"github.com/tamnd/kura/archive"
)

// newRenderCmd builds `kura render <repo>`: re-render the HTML and Markdown views
// from the stored JSON with no network (spec §5, KR3). This is how a renderer
// improvement is replayed over an old archive, and how a Markdown view is added
// to an HTML-only repository.
func newRenderCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "render <repo>",
		Short: "Re-render views from stored JSON, no network",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			f := cmd.Flags()
			views, err := parseViews(mustString(f, "view"))
			if err != nil {
				return err
			}
			date, err := parseDate(mustString(f, "date"))
			if err != nil {
				return fmt.Errorf("parse --date: %w", err)
			}
			res, err := archive.Render(args[0], archive.RenderOptions{
				Views:   views,
				Date:    date,
				Version: Version,
			})
			if err != nil {
				return err
			}
			_, err = fmt.Fprintf(cmd.OutOrStdout(), "rendered %d videos in %s\n", res.Videos, res.Root)
			return err
		},
	}
	f := cmd.Flags()
	f.String("view", "html,md", "views to render: html|md|html,md")
	f.String("date", "", "fix the footer stamp (RFC3339) for reproducible output")
	return cmd
}
