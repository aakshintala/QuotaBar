// The -log flag: file logging for server mode.
package main

import (
	"os"
	"path/filepath"
	"strings"
)

// maxLogBytes rotates the log file past 5 MB.
const maxLogBytes = 5 << 20

// expandHome expands a leading ~/ to the user's home dir, for flags in
// files (like launchd plists) where the shell never runs.
func expandHome(path string) string {
	if path == "~" || strings.HasPrefix(path, "~/") {
		if home, err := os.UserHomeDir(); err == nil {
			if path == "~" {
				return home
			}
			return filepath.Join(home, path[2:])
		}
	}
	return path
}

// rotatedLogPath names the archive: quotad.log becomes quotad.old.log.
func rotatedLogPath(path string) string {
	if strings.HasSuffix(path, ".log") {
		return strings.TrimSuffix(path, ".log") + ".old.log"
	}
	return path + ".old.log"
}

// openLogFile opens path for appending, creating parent dirs. A file over
// 5 MB is renamed aside to the .old.log name first.
func openLogFile(path string) (*os.File, error) {
	if info, err := os.Stat(path); err == nil {
		if info.Size() > maxLogBytes {
			if err := os.Rename(path, rotatedLogPath(path)); err != nil {
				return nil, err
			}
		}
	} else if !os.IsNotExist(err) {
		return nil, err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return nil, err
	}
	return os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
}
