package cli

import (
	"bufio"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/spf13/cobra"
)

// configurable lists every flag whose default can be supplied by the environment
// or the config file. Each entry binds a flag name to its environment variable and
// its config-file key, so resolveDefaults can fill an unset flag from a single
// table. A flag that a given subcommand does not define is skipped.
var configurable = []struct {
	flag string // the cobra flag name
	env  string // the environment variable that overrides the config file
	key  string // the config-file key
}{
	{"out", "KURA_OUT", "out"},
	{"view", "KURA_VIEW", "view"},
	{"depth", "KURA_DEPTH", "depth"},
	{"format", "KURA_FORMAT", "format"},
	{"tool", "KURA_TOOL", "tool"},
	{"ffmpeg-bin", "KURA_FFMPEG", "ffmpeg"},
	{"rate", "KURA_RATE", "rate"},
	{"retries", "KURA_RETRIES", "retries"},
	{"timeout", "KURA_TIMEOUT", "timeout"},
	{"workers", "KURA_WORKERS", "workers"},
	{"hl", "KURA_HL", "hl"},
	{"gl", "KURA_GL", "gl"},
	{"no-cache", "KURA_NO_CACHE", "no_cache"},
}

// resolveDefaults fills each configurable flag the user did not set on the command
// line, in the precedence: an explicit flag wins, else the environment variable,
// else the config file, else the flag's built-in default. It runs as the root
// command's PersistentPreRunE, so every fetching subcommand inherits it.
func resolveDefaults(cmd *cobra.Command, _ []string) error {
	cfg, err := loadConfig()
	if err != nil {
		return err
	}
	flags := cmd.Flags()
	for _, c := range configurable {
		fl := flags.Lookup(c.flag)
		if fl == nil || fl.Changed {
			continue // not on this command, or set explicitly on the command line
		}
		val := os.Getenv(c.env)
		if val == "" {
			val = cfg[c.key]
		}
		if val == "" {
			continue
		}
		if err := fl.Value.Set(val); err != nil {
			return fmt.Errorf("config value for %q: %w", c.key, err)
		}
	}
	return nil
}

// loadConfig reads the optional config file and returns its key/value pairs. A
// missing file is not an error: kura works with zero configuration. The file is a
// plain list of `key = value` lines; blank lines and lines starting with # or ;
// are ignored, and a value may be wrapped in single or double quotes.
func loadConfig() (map[string]string, error) {
	path := configPath()
	if path == "" {
		return nil, nil
	}
	f, err := os.Open(path)
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil
		}
		return nil, fmt.Errorf("open config %s: %w", path, err)
	}
	defer func() { _ = f.Close() }()

	out := make(map[string]string)
	sc := bufio.NewScanner(f)
	for line := 1; sc.Scan(); line++ {
		text := strings.TrimSpace(sc.Text())
		if text == "" || text[0] == '#' || text[0] == ';' {
			continue
		}
		key, value, ok := strings.Cut(text, "=")
		if !ok {
			return nil, fmt.Errorf("%s:%d: want key = value, got %q", path, line, text)
		}
		key = strings.TrimSpace(key)
		value = strings.Trim(strings.TrimSpace(value), `"'`)
		if key == "" {
			return nil, fmt.Errorf("%s:%d: empty key", path, line)
		}
		out[key] = value
	}
	if err := sc.Err(); err != nil {
		return nil, fmt.Errorf("read config %s: %w", path, err)
	}
	return out, nil
}

// configPath returns the config-file path to read, or "" when none applies:
// $KURA_CONFIG if set, else $XDG_CONFIG_HOME/kura/config, else ~/.config/kura/config.
func configPath() string {
	if p := os.Getenv("KURA_CONFIG"); p != "" {
		return p
	}
	if dir := os.Getenv("XDG_CONFIG_HOME"); dir != "" {
		return filepath.Join(dir, "kura", "config")
	}
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		return ""
	}
	return filepath.Join(home, ".config", "kura", "config")
}
