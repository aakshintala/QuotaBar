// Package creds holds credential storage helpers: plain file reads,
// atomic 0600 file writes, and Mac Keychain access through
// /usr/bin/security (the Claude Code item trusts only that tool).
// Secrets are never placed in argv; the write command goes over stdin.
package creds

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"time"
)

// Keychain failure sentinels: security exit 44 is a missing item, exits
// 51 and 128 are user-cancelled or denied access.
var (
	ErrKeychainNotFound = errors.New("creds: keychain item not found")
	ErrKeychainDenied   = errors.New("creds: keychain access denied")
)

// runCommand executes a process and returns its stdout. It is a variable
// so tests can stub the security CLI without touching the real Keychain.
var runCommand = func(ctx context.Context, stdin []byte, name string, args ...string) ([]byte, error) {
	cmd := exec.CommandContext(ctx, name, args...)
	if stdin != nil {
		cmd.Stdin = bytes.NewReader(stdin)
	}

	var stdout bytes.Buffer
	cmd.Stdout = &stdout

	if err := cmd.Run(); err != nil {
		return nil, err
	}

	return stdout.Bytes(), nil
}

// ReadFile returns the file's contents.
func ReadFile(path string) ([]byte, error) {
	return os.ReadFile(path)
}

// WriteFileAtomic writes data with mode 0600 via a temp file in the same
// directory, fsync, and rename, so a crash never leaves a half-written file.
func WriteFileAtomic(path string, data []byte) error {
	dir := filepath.Dir(path)

	tmp, err := os.CreateTemp(dir, ".tmp-*")
	if err != nil {
		return err
	}

	tmpName := tmp.Name()
	defer os.Remove(tmpName) // no-op after a successful rename

	if err := tmp.Chmod(0o600); err != nil {
		tmp.Close()
		return err
	}
	if _, err := tmp.Write(data); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Sync(); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Close(); err != nil {
		return err
	}

	return os.Rename(tmpName, path)
}

func macOnly() error {
	if runtime.GOOS != "darwin" {
		return fmt.Errorf("creds: keychain unavailable on %s", runtime.GOOS)
	}

	return nil
}

// KeychainRead returns the password of the generic item (service, account).
// It runs /usr/bin/security with a 10 s timeout.
func KeychainRead(service, account string) ([]byte, error) {
	if err := macOnly(); err != nil {
		return nil, err
	}

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	out, err := runCommand(ctx, nil, "/usr/bin/security",
		"find-generic-password", "-s", service, "-a", account, "-w")
	if err != nil {
		return nil, mapSecurityError(err)
	}

	if trimmed := bytes.TrimSpace(out); len(trimmed) > 0 {
		return trimmed, nil
	}

	return nil, fmt.Errorf("creds: security read: empty password")
}

// mapSecurityError maps security exit statuses to the Keychain sentinels
// while wrapping the exit error: 44 is a missing item, 51 and 128 are
// user-cancelled or denied access; anything else stays generic.
func mapSecurityError(err error) error {
	var exitErr *exec.ExitError
	if errors.As(err, &exitErr) {
		switch exitErr.ExitCode() {
		case 44:
			return fmt.Errorf("creds: security read: %w: %w", ErrKeychainNotFound, err)
		case 51, 128:
			return fmt.Errorf("creds: security read: %w: %w", ErrKeychainDenied, err)
		}
	}
	return fmt.Errorf("creds: security read: %w", err)
}

// keychainNameOK reports whether s is safe to interpolate into the quoted
// `security -i` script: quotes, backslashes and line breaks would break
// out of the command string or inject a second command.
func keychainNameOK(s string) bool {
	return !strings.ContainsAny(s, "\"\\\n\r")
}

// KeychainWrite creates or updates (-U) the generic item (service, account)
// with data. The secret is hex-encoded (-X) into a command fed to
// `security -i` over stdin, never argv, mirroring the Swift loader.
// Hex digits cannot break out of the command, but service and account are
// interpolated quoted, so hostile values are rejected with an error.
func KeychainWrite(service, account string, data []byte) error {
	if err := macOnly(); err != nil {
		return err
	}
	if !keychainNameOK(service) {
		return fmt.Errorf("creds: keychain write: service contains a quote, backslash or line break")
	}
	if !keychainNameOK(account) {
		return fmt.Errorf("creds: keychain write: account contains a quote, backslash or line break")
	}

	var hex strings.Builder
	for _, b := range data {
		fmt.Fprintf(&hex, "%02x", b)
	}

	command := "add-generic-password -U -a \"" + account + "\" -s \"" + service + "\" -X " +
		hex.String() + "\n"

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	if _, err := runCommand(ctx, []byte(command), "/usr/bin/security", "-i"); err != nil {
		return fmt.Errorf("creds: security write: %w", err)
	}

	return nil
}
