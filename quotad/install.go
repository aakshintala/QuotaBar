// The install and uninstall subcommands (Linux only): set up or remove
// the systemd user unit, settings seed, state dir and aiquota symlink.
package main

import (
	_ "embed"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
)

//go:embed deploy/quotad.service
var unitTemplate string

// defaultSettingsJSON seeds ~/.quotabar/settings.json, matching what
// linux-deploy.sh used to write over ssh.
const defaultSettingsJSON = `{"app":{"backgroundSyncEnabled":true,"backgroundSyncInterval":300},"feed":{"enabled":true,"port":8787}}` + "\n"

// runSystemctl runs systemctl; tests stub it to observe the call sequence.
var runSystemctl = func(args ...string) error {
	cmd := exec.Command("systemctl", args...)
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	return cmd.Run()
}

const linuxOnlyMessage = "quotad %s is Linux-only; on macOS the QuotaBar app runs quotad\n"

func runInstallCommand(stdout, stderr io.Writer) int {
	if runtime.GOOS != "linux" {
		fmt.Fprintf(stderr, linuxOnlyMessage, "install")
		return 2
	}
	home, exe, code, ok := installPaths(stderr)
	if !ok {
		return code
	}
	if err := runInstall(home, exe, stdout); err != nil {
		fmt.Fprintf(stderr, "quotad: install failed: %v\n", err)
		return 1
	}
	return 0
}

func runUninstallCommand(stdout, stderr io.Writer) int {
	if runtime.GOOS != "linux" {
		fmt.Fprintf(stderr, linuxOnlyMessage, "uninstall")
		return 2
	}
	home, exe, code, ok := installPaths(stderr)
	if !ok {
		return code
	}
	if err := runUninstall(home, exe, stdout); err != nil {
		fmt.Fprintf(stderr, "quotad: uninstall failed: %v\n", err)
		return 1
	}
	return 0
}

func installPaths(stderr io.Writer) (home, exe string, code int, ok bool) {
	var err error
	home, err = os.UserHomeDir()
	if err != nil {
		fmt.Fprintf(stderr, "quotad: cannot find home dir: %v\n", err)
		return "", "", 1, false
	}
	exe, err = executablePath()
	if err != nil {
		fmt.Fprintf(stderr, "quotad: cannot resolve executable: %v\n", err)
		return "", "", 1, false
	}
	return home, exe, 0, true
}

// executablePath resolves os.Executable, following symlinks so the unit
// ExecStart and the aiquota symlink point at the real binary.
func executablePath() (string, error) {
	p, err := os.Executable()
	if err != nil {
		return "", err
	}
	resolved, err := filepath.EvalSymlinks(p)
	if err != nil {
		return "", err
	}
	return resolved, nil
}

// unitContent renders the embedded unit template with ExecStart set to
// the installed binary path.
func unitContent(exe string) string {
	var out []string
	for _, line := range strings.Split(unitTemplate, "\n") {
		if strings.HasPrefix(line, "ExecStart=") {
			line = "ExecStart=" + exe
		}
		out = append(out, line)
	}
	return strings.Join(out, "\n")
}

// runInstall writes the unit, seeds settings, fixes the state dir,
// links aiquota and enables the user service.
func runInstall(home, exe string, stdout io.Writer) error {
	unitDir := filepath.Join(home, ".config", "systemd", "user")
	if err := os.MkdirAll(unitDir, 0o755); err != nil {
		return fmt.Errorf("create %s: %w", unitDir, err)
	}
	unit := filepath.Join(unitDir, "quotad.service")
	if err := os.WriteFile(unit, []byte(unitContent(exe)), 0o644); err != nil {
		return fmt.Errorf("write %s: %w", unit, err)
	}
	fmt.Fprintf(stdout, "wrote %s (ExecStart=%s)\n", unit, exe)

	quotabar := filepath.Join(home, ".quotabar")
	if err := os.MkdirAll(quotabar, 0o700); err != nil {
		return fmt.Errorf("create %s: %w", quotabar, err)
	}
	settingsPath := filepath.Join(quotabar, "settings.json")
	if _, err := os.Stat(settingsPath); os.IsNotExist(err) {
		if err := os.WriteFile(settingsPath, []byte(defaultSettingsJSON), 0o600); err != nil {
			return fmt.Errorf("write %s: %w", settingsPath, err)
		}
		fmt.Fprintf(stdout, "created %s\n", settingsPath)
	} else if err != nil {
		return fmt.Errorf("stat %s: %w", settingsPath, err)
	} else {
		fmt.Fprintf(stdout, "kept existing %s\n", settingsPath)
	}

	museDir := filepath.Join(quotabar, "muse")
	if err := os.MkdirAll(museDir, 0o700); err != nil {
		return fmt.Errorf("create %s: %w", museDir, err)
	}
	if err := os.Chmod(museDir, 0o700); err != nil {
		return fmt.Errorf("chmod %s: %w", museDir, err)
	}
	fmt.Fprintf(stdout, "ensured %s (mode 0700)\n", museDir)

	binDir := filepath.Join(home, ".local", "bin")
	if err := os.MkdirAll(binDir, 0o755); err != nil {
		return fmt.Errorf("create %s: %w", binDir, err)
	}
	link := filepath.Join(binDir, "aiquota")
	if _, err := os.Lstat(link); err == nil {
		if err := os.Remove(link); err != nil {
			return fmt.Errorf("remove %s: %w", link, err)
		}
	} else if !os.IsNotExist(err) {
		return fmt.Errorf("stat %s: %w", link, err)
	}
	if err := os.Symlink(exe, link); err != nil {
		return fmt.Errorf("link %s: %w", link, err)
	}
	fmt.Fprintf(stdout, "linked %s -> %s\n", link, exe)

	if err := runSystemctl("--user", "daemon-reload"); err != nil {
		return fmt.Errorf("systemctl --user daemon-reload: %w", err)
	}
	if err := runSystemctl("--user", "enable", "--now", "quotad.service"); err != nil {
		return fmt.Errorf("systemctl --user enable --now quotad.service: %w", err)
	}
	// enable --now is a no-op when already active, so restart in that
	// case for a new binary to take effect.
	if err := runSystemctl("--user", "is-active", "--quiet", "quotad.service"); err == nil {
		if err := runSystemctl("--user", "restart", "quotad.service"); err != nil {
			return fmt.Errorf("systemctl --user restart quotad.service: %w", err)
		}
		fmt.Fprintln(stdout, "restarted quotad.service")
	} else {
		fmt.Fprintln(stdout, "enabled quotad.service")
	}
	return nil
}

// runUninstall stops and removes the user service and the managed
// aiquota symlink, leaving settings and state alone.
func runUninstall(home, exe string, stdout io.Writer) error {
	if err := runSystemctl("--user", "disable", "--now", "quotad.service"); err != nil {
		return fmt.Errorf("systemctl --user disable --now quotad.service: %w", err)
	}
	unit := filepath.Join(home, ".config", "systemd", "user", "quotad.service")
	if err := os.Remove(unit); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("remove %s: %w", unit, err)
	}
	fmt.Fprintf(stdout, "removed %s\n", unit)
	if err := runSystemctl("--user", "daemon-reload"); err != nil {
		return fmt.Errorf("systemctl --user daemon-reload: %w", err)
	}

	link := filepath.Join(home, ".local", "bin", "aiquota")
	if fi, err := os.Lstat(link); err == nil {
		if fi.Mode()&os.ModeSymlink != 0 && symlinkPointsAt(link, exe) {
			if err := os.Remove(link); err != nil {
				return fmt.Errorf("remove %s: %w", link, err)
			}
			fmt.Fprintf(stdout, "removed %s\n", link)
		} else {
			fmt.Fprintf(stdout, "kept %s (not managed by this install)\n", link)
		}
	} else if !os.IsNotExist(err) {
		return fmt.Errorf("stat %s: %w", link, err)
	}
	return nil
}

// symlinkPointsAt reports whether link resolves to exe.
func symlinkPointsAt(link, exe string) bool {
	target, err := filepath.EvalSymlinks(link)
	if err != nil {
		return false
	}
	want := exe
	if resolved, err := filepath.EvalSymlinks(exe); err == nil {
		want = resolved
	}
	return target == want
}
