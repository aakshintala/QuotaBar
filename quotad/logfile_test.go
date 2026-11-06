package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestExpandHome(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	home, _ := os.UserHomeDir()
	if got := expandHome("~/a/b"); got != filepath.Join(home, "a", "b") {
		t.Errorf("expandHome(~/a/b) = %q, want under %q", got, home)
	}
	if got := expandHome("/abs/path"); got != "/abs/path" {
		t.Errorf("expandHome(/abs/path) = %q, want unchanged", got)
	}
	if got := expandHome("rel/path"); got != "rel/path" {
		t.Errorf("expandHome(rel/path) = %q, want unchanged", got)
	}
}

func TestRotatedLogPath(t *testing.T) {
	if got := rotatedLogPath("/x/quotad.log"); got != "/x/quotad.old.log" {
		t.Errorf("rotated = %q", got)
	}
	if got := rotatedLogPath("/x/quotad.txt"); got != "/x/quotad.txt.old.log" {
		t.Errorf("rotated = %q", got)
	}
}

func TestOpenLogFileRotatesOver5MB(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "logs", "quotad.log")
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	big := strings.Repeat("x", maxLogBytes+1)
	if err := os.WriteFile(path, []byte(big), 0o644); err != nil {
		t.Fatal(err)
	}
	f, err := openLogFile(path)
	if err != nil {
		t.Fatal(err)
	}
	f.Close()
	old, err := os.ReadFile(rotatedLogPath(path))
	if err != nil {
		t.Fatalf("rotated file missing: %v", err)
	}
	if string(old) != big {
		t.Error("rotated file does not hold the previous content")
	}
	if fi, err := os.Stat(path); err != nil || fi.Size() != 0 {
		t.Errorf("fresh log missing or not empty: %v size", fi)
	}
}

func TestOpenLogFileAppends(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "sub", "quotad.log") // sub dir does not exist yet
	f, err := openLogFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := f.WriteString("a"); err != nil {
		t.Fatal(err)
	}
	f.Close()
	f, err = openLogFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := f.WriteString("b"); err != nil {
		t.Fatal(err)
	}
	f.Close()
	got, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if string(got) != "ab" {
		t.Errorf("log = %q, want %q", got, "ab")
	}
}
