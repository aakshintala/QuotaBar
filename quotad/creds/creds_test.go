package creds

import (
	"bytes"
	"context"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
)

func TestReadFileRoundTrip(t *testing.T) {
	path := filepath.Join(t.TempDir(), "auth.json")
	if err := os.WriteFile(path, []byte(`{"a":1}`), 0o644); err != nil {
		t.Fatal(err)
	}
	data, err := ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if string(data) != `{"a":1}` {
		t.Errorf("ReadFile = %q", data)
	}
}

func TestReadFileMissingErrors(t *testing.T) {
	if _, err := ReadFile(filepath.Join(t.TempDir(), "nope.json")); err == nil {
		t.Error("expected an error for a missing file")
	}
}

func TestWriteFileAtomicSets0600(t *testing.T) {
	path := filepath.Join(t.TempDir(), "creds.json")
	if err := WriteFileAtomic(path, []byte("secret")); err != nil {
		t.Fatal(err)
	}
	data, err := os.ReadFile(path)
	if err != nil || string(data) != "secret" {
		t.Fatalf("content = %q, err = %v", data, err)
	}
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	if perm := info.Mode().Perm(); perm != 0o600 {
		t.Errorf("mode = %o, want 600", perm)
	}
	entries, err := os.ReadDir(filepath.Dir(path))
	if err != nil {
		t.Fatal(err)
	}
	for _, e := range entries {
		if strings.HasPrefix(e.Name(), ".tmp-") {
			t.Errorf("stray temp file left behind: %s", e.Name())
		}
	}
}

func TestWriteFileAtomicOverwrites(t *testing.T) {
	path := filepath.Join(t.TempDir(), "creds.json")
	if err := WriteFileAtomic(path, []byte("one")); err != nil {
		t.Fatal(err)
	}
	if err := WriteFileAtomic(path, []byte("two")); err != nil {
		t.Fatal(err)
	}
	data, _ := os.ReadFile(path)
	if string(data) != "two" {
		t.Errorf("content = %q, want two", data)
	}
}

func stubRun(t *testing.T, out []byte, err error, capture *struct {
	name  string
	args  []string
	stdin []byte
}) {
	t.Helper()
	old := runCommand
	runCommand = func(_ context.Context, stdin []byte, name string, args ...string) ([]byte, error) {
		capture.name, capture.args, capture.stdin = name, args, stdin
		return out, err
	}
	t.Cleanup(func() { runCommand = old })
}

func TestKeychainOffDarwinErrors(t *testing.T) {
	if runtime.GOOS == "darwin" {
		t.Skip("darwin-only negative path")
	}
	if _, err := KeychainRead("s", "a"); err == nil {
		t.Error("KeychainRead should fail off darwin")
	}
	if err := KeychainWrite("s", "a", []byte("x")); err == nil {
		t.Error("KeychainWrite should fail off darwin")
	}
}

func TestKeychainReadInvokesSecurity(t *testing.T) {
	if runtime.GOOS != "darwin" {
		t.Skip("needs darwin guard to pass")
	}
	var got struct {
		name  string
		args  []string
		stdin []byte
	}
	stubRun(t, []byte("pw\n"), nil, &got)
	out, err := KeychainRead("svc", "acct")
	if err != nil {
		t.Fatal(err)
	}
	if string(out) != "pw" {
		t.Errorf("output = %q, want trimmed pw", out)
	}
	if got.name != "/usr/bin/security" {
		t.Errorf("binary = %q", got.name)
	}
	want := []string{"find-generic-password", "-s", "svc", "-a", "acct", "-w"}
	if len(got.args) != len(want) {
		t.Fatalf("args = %v", got.args)
	}
	for i := range want {
		if got.args[i] != want[i] {
			t.Errorf("args = %v, want %v", got.args, want)
			break
		}
	}
	if got.stdin != nil {
		t.Error("read must not send stdin")
	}
}

func TestKeychainWriteRejectsHostileNames(t *testing.T) {
	if runtime.GOOS != "darwin" {
		t.Skip("needs darwin guard to pass")
	}
	hostile := []string{`a"b`, `a\b`, "a\nb", "a\rb"}
	for _, name := range hostile {
		for _, tc := range []struct {
			field   string
			service string
			account string
		}{
			{"service", name, "acct"},
			{"account", "svc", name},
		} {
			var got struct {
				name  string
				args  []string
				stdin []byte
			}
			stubRun(t, nil, nil, &got)
			if err := KeychainWrite(tc.service, tc.account, []byte("x")); err == nil {
				t.Errorf("%s %q: expected an error", tc.field, name)
			}
			if got.name != "" {
				t.Errorf("%s %q: hostile name reached the security CLI", tc.field, name)
			}
		}
	}
}

func TestKeychainWriteHexEncodesHostileSecret(t *testing.T) {
	if runtime.GOOS != "darwin" {
		t.Skip("needs darwin guard to pass")
	}
	var got struct {
		name  string
		args  []string
		stdin []byte
	}
	stubRun(t, nil, nil, &got)
	secret := []byte("x\"y\\z\nw;rm -rf ~$(id)")
	if err := KeychainWrite("svc", "acct", secret); err != nil {
		t.Fatal(err)
	}
	want := "add-generic-password -U -a \"acct\" -s \"svc\" -X 7822795c7a0a773b726d202d7266207e2428696429\n"
	if string(got.stdin) != want {
		t.Errorf("stdin = %q, want %q", got.stdin, want)
	}
}

func TestKeychainWriteUsesStdin(t *testing.T) {
	if runtime.GOOS != "darwin" {
		t.Skip("needs darwin guard to pass")
	}
	var got struct {
		name  string
		args  []string
		stdin []byte
	}
	stubRun(t, nil, nil, &got)
	if err := KeychainWrite("svc", "acct", []byte("hi")); err != nil {
		t.Fatal(err)
	}
	if got.name != "/usr/bin/security" || len(got.args) != 1 || got.args[0] != "-i" {
		t.Errorf("invocation = %q %v, want security -i", got.name, got.args)
	}
	// "hi" hex-encoded is 6869; the secret itself must not appear in argv.
	want := "add-generic-password -U -a \"acct\" -s \"svc\" -X 6869\n"
	if string(got.stdin) != want {
		t.Errorf("stdin = %q, want %q", got.stdin, want)
	}
	for _, a := range got.args {
		if strings.Contains(a, "6869") || strings.Contains(a, "hi") {
			t.Errorf("secret leaked into argv: %v", got.args)
		}
	}
	if bytes.Contains(got.stdin, []byte("hi")) {
		t.Errorf("raw secret should be hex-encoded, stdin = %q", got.stdin)
	}
}
