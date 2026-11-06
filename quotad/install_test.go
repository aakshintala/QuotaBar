package main

import (
	"bytes"
	"errors"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
)

func tempHome(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	resolved, err := filepath.EvalSymlinks(dir)
	if err != nil {
		t.Fatal(err)
	}
	return resolved
}

func writeExe(t *testing.T, home string) string {
	t.Helper()
	exe := filepath.Join(home, ".local", "bin", "quotad")
	if err := os.MkdirAll(filepath.Dir(exe), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(exe, []byte("binary"), 0o755); err != nil {
		t.Fatal(err)
	}
	return exe
}

func stubSystemctl(t *testing.T, calls *[]string, isActiveErr error) {
	t.Helper()
	old := runSystemctl
	*calls = nil
	runSystemctl = func(args ...string) error {
		*calls = append(*calls, strings.Join(args, " "))
		for _, a := range args {
			if a == "is-active" {
				return isActiveErr
			}
		}
		return nil
	}
	t.Cleanup(func() { runSystemctl = old })
}

func unitPath(home string) string {
	return filepath.Join(home, ".config", "systemd", "user", "quotad.service")
}

func TestInstallWritesUnitFromTemplate(t *testing.T) {
	home := tempHome(t)
	exe := writeExe(t, home)
	var calls []string
	stubSystemctl(t, &calls, errors.New("inactive"))
	var out bytes.Buffer
	if err := runInstall(home, exe, &out); err != nil {
		t.Fatalf("runInstall: %v", err)
	}
	raw, err := os.ReadFile("deploy/quotad.service")
	if err != nil {
		t.Fatal(err)
	}
	want := strings.Replace(string(raw), "ExecStart=%h/.local/bin/quotad", "ExecStart="+exe, 1)
	got, err := os.ReadFile(unitPath(home))
	if err != nil {
		t.Fatal(err)
	}
	if string(got) != want {
		t.Errorf("unit = %q, want template with ExecStart=%s", got, exe)
	}
	if !strings.Contains(out.String(), unitPath(home)) {
		t.Errorf("install output should name the unit, got %q", out.String())
	}
}

func TestInstallCreatesSettingsOnlyWhenMissing(t *testing.T) {
	home := tempHome(t)
	exe := writeExe(t, home)
	var calls []string
	stubSystemctl(t, &calls, errors.New("inactive"))
	settings := filepath.Join(home, ".quotabar", "settings.json")
	if err := runInstall(home, exe, &bytes.Buffer{}); err != nil {
		t.Fatalf("runInstall: %v", err)
	}
	got, err := os.ReadFile(settings)
	if err != nil {
		t.Fatal(err)
	}
	if string(got) != defaultSettingsJSON {
		t.Errorf("settings = %q, want %q", got, defaultSettingsJSON)
	}
	const custom = "{\"feed\":{\"port\":9999}}\n"
	if err := os.WriteFile(settings, []byte(custom), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := runInstall(home, exe, &bytes.Buffer{}); err != nil {
		t.Fatalf("second runInstall: %v", err)
	}
	kept, err := os.ReadFile(settings)
	if err != nil {
		t.Fatal(err)
	}
	if string(kept) != custom {
		t.Errorf("existing settings overwritten: %q", kept)
	}
}

func TestInstallReplacesRegularAiquotaWithSymlink(t *testing.T) {
	home := tempHome(t)
	exe := writeExe(t, home)
	var calls []string
	stubSystemctl(t, &calls, errors.New("inactive"))
	link := filepath.Join(home, ".local", "bin", "aiquota")
	if err := os.WriteFile(link, []byte("#!/bin/sh\nexec old\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := runInstall(home, exe, &bytes.Buffer{}); err != nil {
		t.Fatalf("runInstall: %v", err)
	}
	fi, err := os.Lstat(link)
	if err != nil {
		t.Fatal(err)
	}
	if fi.Mode()&os.ModeSymlink == 0 {
		t.Fatalf("aiquota is %v, want a symlink", fi.Mode())
	}
	target, err := os.Readlink(link)
	if err != nil {
		t.Fatal(err)
	}
	if target != exe {
		t.Errorf("aiquota -> %q, want %q", target, exe)
	}
}

func TestInstallChmodsClaudeDir(t *testing.T) {
	home := tempHome(t)
	exe := writeExe(t, home)
	var calls []string
	stubSystemctl(t, &calls, errors.New("inactive"))
	museDir := filepath.Join(home, ".quotabar", "muse")
	if err := os.MkdirAll(museDir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := runInstall(home, exe, &bytes.Buffer{}); err != nil {
		t.Fatalf("runInstall: %v", err)
	}
	fi, err := os.Stat(museDir)
	if err != nil {
		t.Fatal(err)
	}
	if fi.Mode().Perm() != 0o700 {
		t.Errorf("muse mode = %o, want 700", fi.Mode().Perm())
	}
}

func TestInstallSystemctlSequenceRestartsWhenActive(t *testing.T) {
	home := tempHome(t)
	exe := writeExe(t, home)
	var calls []string
	stubSystemctl(t, &calls, nil) // is-active succeeds: already running
	if err := runInstall(home, exe, &bytes.Buffer{}); err != nil {
		t.Fatalf("runInstall: %v", err)
	}
	want := []string{
		"--user daemon-reload",
		"--user enable --now quotad.service",
		"--user is-active --quiet quotad.service",
		"--user restart quotad.service",
	}
	if strings.Join(calls, "\n") != strings.Join(want, "\n") {
		t.Errorf("systemctl calls = %q, want %q", calls, want)
	}
}

func TestInstallSystemctlSequenceSkipsRestartWhenInactive(t *testing.T) {
	home := tempHome(t)
	exe := writeExe(t, home)
	var calls []string
	stubSystemctl(t, &calls, errors.New("inactive"))
	if err := runInstall(home, exe, &bytes.Buffer{}); err != nil {
		t.Fatalf("runInstall: %v", err)
	}
	want := []string{
		"--user daemon-reload",
		"--user enable --now quotad.service",
		"--user is-active --quiet quotad.service",
	}
	if strings.Join(calls, "\n") != strings.Join(want, "\n") {
		t.Errorf("systemctl calls = %q, want %q", calls, want)
	}
}

func TestInstallFailsWhenSystemctlFails(t *testing.T) {
	home := tempHome(t)
	exe := writeExe(t, home)
	old := runSystemctl
	runSystemctl = func(args ...string) error { return errors.New("no dbus") }
	t.Cleanup(func() { runSystemctl = old })
	if err := runInstall(home, exe, &bytes.Buffer{}); err == nil {
		t.Error("runInstall with failing systemctl should return an error")
	}
}

func TestInstallLinuxOnlyMessage(t *testing.T) {
	if runtime.GOOS == "linux" {
		t.Skip("Linux-only gate only fires off Linux")
	}
	var stdout, stderr bytes.Buffer
	if code := run([]string{"quotad", "install"}, &stdout, &stderr); code != 2 {
		t.Fatalf("exit = %d, want 2 (stderr = %q)", code, stderr.String())
	}
	want := "quotad install is Linux-only; on macOS the QuotaBar app runs quotad\n"
	if stderr.String() != want {
		t.Errorf("stderr = %q, want %q", stderr.String(), want)
	}
}

func TestUninstallLinuxOnlyMessage(t *testing.T) {
	if runtime.GOOS == "linux" {
		t.Skip("Linux-only gate only fires off Linux")
	}
	var stdout, stderr bytes.Buffer
	if code := run([]string{"quotad", "uninstall"}, &stdout, &stderr); code != 2 {
		t.Fatalf("exit = %d, want 2 (stderr = %q)", code, stderr.String())
	}
	want := "quotad uninstall is Linux-only; on macOS the QuotaBar app runs quotad\n"
	if stderr.String() != want {
		t.Errorf("stderr = %q, want %q", stderr.String(), want)
	}
}

func TestUninstallRemovesManagedFiles(t *testing.T) {
	home := tempHome(t)
	exe := writeExe(t, home)
	var calls []string
	stubSystemctl(t, &calls, errors.New("inactive"))
	if err := runInstall(home, exe, &bytes.Buffer{}); err != nil {
		t.Fatalf("runInstall: %v", err)
	}
	settings := filepath.Join(home, ".quotabar", "settings.json")
	calls = nil
	var out bytes.Buffer
	if err := runUninstall(home, exe, &out); err != nil {
		t.Fatalf("runUninstall: %v", err)
	}
	if _, err := os.Stat(unitPath(home)); !os.IsNotExist(err) {
		t.Errorf("unit still present: %v", err)
	}
	if _, err := os.Lstat(filepath.Join(home, ".local", "bin", "aiquota")); !os.IsNotExist(err) {
		t.Errorf("managed aiquota still present: %v", err)
	}
	if _, err := os.Stat(settings); err != nil {
		t.Errorf("settings should be left alone: %v", err)
	}
	want := []string{
		"--user disable --now quotad.service",
		"--user daemon-reload",
	}
	if strings.Join(calls, "\n") != strings.Join(want, "\n") {
		t.Errorf("systemctl calls = %q, want %q", calls, want)
	}
}

func TestUninstallLeavesForeignAiquota(t *testing.T) {
	home := tempHome(t)
	exe := writeExe(t, home)
	var calls []string
	stubSystemctl(t, &calls, errors.New("inactive"))
	link := filepath.Join(home, ".local", "bin", "aiquota")

	// Regular file.
	if err := os.WriteFile(link, []byte("foreign"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := runUninstall(home, exe, &bytes.Buffer{}); err != nil {
		t.Fatalf("runUninstall: %v", err)
	}
	if got, err := os.ReadFile(link); err != nil || string(got) != "foreign" {
		t.Errorf("foreign regular aiquota touched: %q, %v", got, err)
	}

	// Symlink elsewhere.
	other := filepath.Join(home, "other-quotad")
	if err := os.WriteFile(other, []byte("x"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(link); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(other, link); err != nil {
		t.Fatal(err)
	}
	if err := runUninstall(home, exe, &bytes.Buffer{}); err != nil {
		t.Fatalf("runUninstall: %v", err)
	}
	target, err := os.Readlink(link)
	if err != nil || target != other {
		t.Errorf("foreign symlink touched: %q, %v", target, err)
	}
}
