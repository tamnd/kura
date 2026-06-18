package cli

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/spf13/cobra"
)

func TestLoadConfig(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "config")
	body := "# a comment\n; another\n\nout = /vault\ndepth = media\nview = \"html,md\"\nrate = 750ms\n"
	if err := os.WriteFile(path, []byte(body), 0o644); err != nil {
		t.Fatal(err)
	}
	t.Setenv("KURA_CONFIG", path)

	cfg, err := loadConfig()
	if err != nil {
		t.Fatalf("loadConfig: %v", err)
	}
	want := map[string]string{"out": "/vault", "depth": "media", "view": "html,md", "rate": "750ms"}
	for k, v := range want {
		if cfg[k] != v {
			t.Errorf("cfg[%q] = %q, want %q", k, cfg[k], v)
		}
	}
}

func TestLoadConfigMissingIsNotAnError(t *testing.T) {
	t.Setenv("KURA_CONFIG", filepath.Join(t.TempDir(), "absent"))
	cfg, err := loadConfig()
	if err != nil {
		t.Fatalf("missing config should be silent, got %v", err)
	}
	if len(cfg) != 0 {
		t.Errorf("missing config should yield no keys, got %v", cfg)
	}
}

func TestLoadConfigMalformed(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "config")
	if err := os.WriteFile(path, []byte("this line has no equals\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	t.Setenv("KURA_CONFIG", path)
	if _, err := loadConfig(); err == nil {
		t.Error("a line without = should error")
	}
}

func TestResolveDefaultsPrecedence(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "config")
	if err := os.WriteFile(path, []byte("out = /from-config\ndepth = media\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	t.Setenv("KURA_CONFIG", path)
	t.Setenv("KURA_OUT", "/from-env")

	newCmd := func() *cobra.Command {
		cmd := &cobra.Command{Use: "x", RunE: func(*cobra.Command, []string) error { return nil }}
		cmd.Flags().String("out", "/builtin", "")
		cmd.Flags().String("depth", "meta", "")
		cmd.Flags().String("view", "html", "")
		return cmd
	}

	// env beats config, config beats built-in, untouched key keeps its default.
	cmd := newCmd()
	if err := resolveDefaults(cmd, nil); err != nil {
		t.Fatalf("resolveDefaults: %v", err)
	}
	if got, _ := cmd.Flags().GetString("out"); got != "/from-env" {
		t.Errorf("out = %q, want /from-env (env wins)", got)
	}
	if got, _ := cmd.Flags().GetString("depth"); got != "media" {
		t.Errorf("depth = %q, want media (config beats built-in)", got)
	}
	if got, _ := cmd.Flags().GetString("view"); got != "html" {
		t.Errorf("view = %q, want html (built-in default)", got)
	}

	// An explicit flag beats both env and config.
	cmd = newCmd()
	if err := cmd.Flags().Set("out", "/from-flag"); err != nil {
		t.Fatal(err)
	}
	if err := resolveDefaults(cmd, nil); err != nil {
		t.Fatalf("resolveDefaults: %v", err)
	}
	if got, _ := cmd.Flags().GetString("out"); got != "/from-flag" {
		t.Errorf("out = %q, want /from-flag (explicit flag wins)", got)
	}
}
