package opencode

import (
	"context"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"
)

// testEnv builds a LookupEnv from key/value pairs; missing keys read as unset.
func testEnv(pairs ...string) func(string) (string, bool) {
	m := map[string]string{}
	for i := 0; i+1 < len(pairs); i += 2 {
		m[pairs[i]] = pairs[i+1]
	}
	return func(key string) (string, bool) {
		v, ok := m[key]
		return v, ok
	}
}

// stubSQLite replaces runSQLite for one test, failing on unexpected calls
// unless handle is set.
func stubSQLite(t *testing.T, handle func(dbPath, sql string) ([]byte, []byte, error)) {
	t.Helper()
	orig := runSQLite
	runSQLite = func(_ context.Context, dbPath, sql string) ([]byte, []byte, error) {
		if handle == nil {
			t.Errorf("runSQLite called unexpectedly (db %s)", dbPath)
			return nil, nil, errors.New("unexpected sqlite call")
		}
		return handle(dbPath, sql)
	}
	t.Cleanup(func() { runSQLite = orig })
}

// writeAuthFile writes an auth.json with one opencode-go key entry.
func writeAuthFile(t *testing.T, dir, key string) {
	t.Helper()
	opencodeDir := filepath.Join(dir, "opencode")
	if err := os.MkdirAll(opencodeDir, 0o755); err != nil {
		t.Fatal(err)
	}
	body := `{"opencode-go": {"type": "api", "key": ` + strconv.Quote(key) + `}}`
	if err := os.WriteFile(filepath.Join(opencodeDir, "auth.json"), []byte(body), 0o600); err != nil {
		t.Fatal(err)
	}
}

func TestDataDir(t *testing.T) {
	p := New(WithHome("/home/u"), WithLookupEnv(testEnv("XDG_DATA_HOME", "/data")))
	if got := p.dataDir(); got != filepath.Join("/data", "opencode") {
		t.Fatalf("absolute XDG: got %s", got)
	}

	p = New(WithHome("/home/u"), WithLookupEnv(testEnv()))
	if got, want := p.dataDir(), filepath.Join("/home/u", ".local", "share", "opencode"); got != want {
		t.Fatalf("unset XDG: got %s want %s", got, want)
	}

	p = New(WithHome("/home/u"), WithLookupEnv(testEnv("XDG_DATA_HOME", "relative/path")))
	if got, want := p.dataDir(), filepath.Join("/home/u", ".local", "share", "opencode"); got != want {
		t.Fatalf("relative XDG: got %s want %s", got, want)
	}

	p = New(WithHome("/home/u"), WithLookupEnv(testEnv("XDG_DATA_HOME", "")))
	if got, want := p.dataDir(), filepath.Join("/home/u", ".local", "share", "opencode"); got != want {
		t.Fatalf("empty XDG: got %s want %s", got, want)
	}
}

func TestDBPath(t *testing.T) {
	p := New(WithHome("/h"), WithLookupEnv(testEnv()))
	if got, want := p.dbPath(), filepath.Join("/h", ".local", "share", "opencode", "opencode.db"); got != want {
		t.Fatalf("default: got %s want %s", got, want)
	}

	p = New(WithHome("/h"), WithLookupEnv(testEnv("OPENCODE_DB", "/abs/custom.db")))
	if got := p.dbPath(); got != "/abs/custom.db" {
		t.Fatalf("absolute OPENCODE_DB: got %s", got)
	}

	p = New(WithHome("/h"), WithLookupEnv(testEnv("OPENCODE_DB", "custom.db")))
	if got, want := p.dbPath(), filepath.Join("/h", ".local", "share", "opencode", "custom.db"); got != want {
		t.Fatalf("relative OPENCODE_DB: got %s want %s", got, want)
	}

	p = New(WithHome("/h"), WithLookupEnv(testEnv("OPENCODE_DB", ":memory:")))
	if got := p.dbPath(); got != "" {
		t.Fatalf(":memory: should disable the database source, got %s", got)
	}
}

func TestLoadFileKey(t *testing.T) {
	home := t.TempDir()
	writeAuthFile(t, filepath.Join(home, ".local", "share"), "sk-test-key")
	p := New(WithHome(home), WithLookupEnv(testEnv()))
	key, err := p.loadFileKey()
	if err != nil || key != "sk-test-key" {
		t.Fatalf("got %q, %v", key, err)
	}
}

func TestLoadFileKeyMissing(t *testing.T) {
	p := New(WithHome(t.TempDir()), WithLookupEnv(testEnv()))
	key, err := p.loadFileKey()
	if err != nil || key != "" {
		t.Fatalf("missing file should give no key: got %q, %v", key, err)
	}
}

func TestLoadFileKeyMissingEntry(t *testing.T) {
	home := t.TempDir()
	dir := filepath.Join(home, ".local", "share", "opencode")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	body := `{"some-other-provider": {"type": "api", "key": "x"}}`
	if err := os.WriteFile(filepath.Join(dir, "auth.json"), []byte(body), 0o600); err != nil {
		t.Fatal(err)
	}
	p := New(WithHome(home), WithLookupEnv(testEnv()))
	key, err := p.loadFileKey()
	if err != nil || key != "" {
		t.Fatalf("missing entry should give no key: got %q, %v", key, err)
	}
}

func TestLoadKeyPrefersDatabase(t *testing.T) {
	home := t.TempDir()
	writeAuthFile(t, filepath.Join(home, ".local", "share"), "sk-file-key")
	if err := os.WriteFile(
		filepath.Join(home, ".local", "share", "opencode", "opencode.db"), []byte{}, 0o600,
	); err != nil {
		t.Fatal(err)
	}
	stubSQLite(t, func(dbPath, sql string) ([]byte, []byte, error) {
		if !strings.Contains(sql, "integration_id") || !strings.Contains(sql, "active = 1") {
			t.Errorf("unexpected SQL: %s", sql)
		}
		if !strings.Contains(sql, "'opencode-go'") || !strings.Contains(sql, "'opencode'") {
			t.Errorf("SQL must prefer opencode-go over opencode: %s", sql)
		}
		// Both integrations present, in the ORDER BY preference order: the
		// preferred opencode-go key must win, not an arbitrary row.
		out := "opencode-go\x1f" + `{"type":"key","key":"sk-db-key"}` + "\n" +
			"opencode\x1f" + `{"type":"key","key":"[REDACTED]"}` + "\n"
		return []byte(out), nil, nil
	})
	p := New(WithHome(home), WithLookupEnv(testEnv()))
	key, err := p.loadKey(context.Background())
	if err != nil || key != "sk-db-key" {
		t.Fatalf("preferred opencode-go key should win: got %q, %v", key, err)
	}
}

func TestLoadKeyFallsBackToOpencodeIntegration(t *testing.T) {
	home := t.TempDir()
	if err := os.MkdirAll(filepath.Join(home, ".local", "share", "opencode"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(
		filepath.Join(home, ".local", "share", "opencode", "opencode.db"), []byte{}, 0o600,
	); err != nil {
		t.Fatal(err)
	}
	stubSQLite(t, func(dbPath, sql string) ([]byte, []byte, error) {
		out := "opencode\x1f" + `{"type":"key","key":"sk-opencode-key"}` + "\n"
		return []byte(out), nil, nil
	})
	p := New(WithHome(home), WithLookupEnv(testEnv()))
	key, err := p.loadKey(context.Background())
	if err != nil || key != "sk-opencode-key" {
		t.Fatalf("got %q, %v", key, err)
	}
}

func TestLoadKeyOAuthCredential(t *testing.T) {
	home := t.TempDir()
	if err := os.MkdirAll(filepath.Join(home, ".local", "share", "opencode"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(
		filepath.Join(home, ".local", "share", "opencode", "opencode.db"), []byte{}, 0o600,
	); err != nil {
		t.Fatal(err)
	}
	stubSQLite(t, func(dbPath, sql string) ([]byte, []byte, error) {
		out := "opencode-go\x1f" + `{"type":"oauth","access":"sk-oauth-key","expires":9999999999000}` + "\n"
		return []byte(out), nil, nil
	})
	p := New(WithHome(home), WithLookupEnv(testEnv()))
	key, err := p.loadKey(context.Background())
	if err != nil || key != "sk-oauth-key" {
		t.Fatalf("got %q, %v", key, err)
	}
}

func TestLoadKeyExpiredOAuthFails(t *testing.T) {
	home := t.TempDir()
	if err := os.MkdirAll(filepath.Join(home, ".local", "share", "opencode"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(
		filepath.Join(home, ".local", "share", "opencode", "opencode.db"), []byte{}, 0o600,
	); err != nil {
		t.Fatal(err)
	}
	stubSQLite(t, func(dbPath, sql string) ([]byte, []byte, error) {
		out := "opencode-go\x1f" + `{"type":"oauth","access":"sk-old","expires":1000}` + "\n"
		return []byte(out), nil, nil
	})
	p := New(WithHome(home), WithLookupEnv(testEnv()))
	if _, err := p.loadKey(context.Background()); err == nil {
		t.Fatal("expired oauth credential should fail")
	}
}

func TestLoadKeyAmbiguousRowsFail(t *testing.T) {
	home := t.TempDir()
	if err := os.MkdirAll(filepath.Join(home, ".local", "share", "opencode"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(
		filepath.Join(home, ".local", "share", "opencode", "opencode.db"), []byte{}, 0o600,
	); err != nil {
		t.Fatal(err)
	}
	stubSQLite(t, func(dbPath, sql string) ([]byte, []byte, error) {
		out := "opencode-go\x1f" + `{"type":"key","key":"a"}` + "\n" +
			"opencode-go\x1f" + `{"type":"key","key":"b"}` + "\n"
		return []byte(out), nil, nil
	})
	p := New(WithHome(home), WithLookupEnv(testEnv()))
	if _, err := p.loadKey(context.Background()); err == nil {
		t.Fatal("two active rows for one integration should fail")
	}
}

func TestLoadKeyMissingTableFallsBackToFile(t *testing.T) {
	home := t.TempDir()
	writeAuthFile(t, filepath.Join(home, ".local", "share"), "sk-file-key")
	if err := os.WriteFile(
		filepath.Join(home, ".local", "share", "opencode", "opencode.db"), []byte{}, 0o600,
	); err != nil {
		t.Fatal(err)
	}
	stubSQLite(t, func(dbPath, sql string) ([]byte, []byte, error) {
		return nil, []byte("Error: no such table: credential"), errors.New("exit status 1")
	})
	p := New(WithHome(home), WithLookupEnv(testEnv()))
	key, err := p.loadKey(context.Background())
	if err != nil || key != "sk-file-key" {
		t.Fatalf("got %q, %v", key, err)
	}
}

func TestLoadKeyInaccessibleDatabaseFailsClosed(t *testing.T) {
	home := t.TempDir()
	writeAuthFile(t, filepath.Join(home, ".local", "share"), "[REDACTED]")
	opencodeDir := filepath.Join(home, ".local", "share", "opencode")
	blocker := filepath.Join(opencodeDir, "blocker")
	if err := os.WriteFile(blocker, []byte{}, 0o600); err != nil {
		t.Fatal(err)
	}
	// Stat fails with ENOTDIR here, not "not exist": the database is
	// present but inaccessible, so falling back to auth.json could select
	// a stale key. Fail closed instead.
	p := New(WithHome(home), WithLookupEnv(testEnv(
		"OPENCODE_DB", filepath.Join(blocker, "opencode.db"),
	)))
	stubSQLite(t, nil)
	key, err := p.loadKey(context.Background())
	if err == nil || key != "" {
		t.Fatalf("inaccessible database must fail closed: got %q, %v", key, err)
	}
}

func TestOAuthExpiryUsesFractionalNow(t *testing.T) {
	now := time.Unix(1_754_000_000, 700*1_000_000)
	// 0.2 s past expiry but within the same floored second: a floored
	// comparison would wrongly accept this token.
	if !oauthExpired(float64(1_754_000_000)*1000+500, now) {
		t.Fatal("token 0.2s past expiry should count as expired")
	}
	if oauthExpired(float64(1_754_000_000)*1000+800, now) {
		t.Fatal("token 0.1s before expiry should still be valid")
	}
}

func TestLoadKeyDatabaseErrorFailsClosed(t *testing.T) {
	home := t.TempDir()
	if err := os.MkdirAll(filepath.Join(home, ".local", "share", "opencode"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(
		filepath.Join(home, ".local", "share", "opencode", "opencode.db"), []byte{}, 0o600,
	); err != nil {
		t.Fatal(err)
	}
	stubSQLite(t, func(dbPath, sql string) ([]byte, []byte, error) {
		return nil, []byte("Error: disk I/O error"), errors.New("exit status 1")
	})
	p := New(WithHome(home), WithLookupEnv(testEnv()))
	key, err := p.loadKey(context.Background())
	if err == nil || key != "" {
		t.Fatalf("database errors must fail closed: got %q, %v", key, err)
	}
	if strings.Contains(err.Error(), "I/O") {
		t.Fatalf("database details must not leak: %v", err)
	}
}

func TestLoadKeyRejectsTokenWithWhitespace(t *testing.T) {
	home := t.TempDir()
	if err := os.MkdirAll(filepath.Join(home, ".local", "share", "opencode"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(
		filepath.Join(home, ".local", "share", "opencode", "opencode.db"), []byte{}, 0o600,
	); err != nil {
		t.Fatal(err)
	}
	stubSQLite(t, func(dbPath, sql string) ([]byte, []byte, error) {
		out := "opencode-go\x1f" + `{"type":"key","key":"bad key"}` + "\n"
		return []byte(out), nil, nil
	})
	p := New(WithHome(home), WithLookupEnv(testEnv()))
	if _, err := p.loadKey(context.Background()); err == nil {
		t.Fatal("token with whitespace should fail")
	}
}

func TestAvailableChecksOnlyStats(t *testing.T) {
	home := t.TempDir()
	p := New(WithHome(home), WithLookupEnv(testEnv()))
	stubSQLite(t, nil)
	if p.Available() {
		t.Fatal("no credential source: Available should be false")
	}

	writeAuthFile(t, filepath.Join(home, ".local", "share"), "sk-test-key")
	if !p.Available() {
		t.Fatal("auth.json present: Available should be true")
	}
}

func TestAvailableTrueWhenDatabaseExists(t *testing.T) {
	home := t.TempDir()
	if err := os.MkdirAll(filepath.Join(home, ".local", "share", "opencode"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(
		filepath.Join(home, ".local", "share", "opencode", "opencode.db"), []byte{}, 0o600,
	); err != nil {
		t.Fatal(err)
	}
	p := New(WithHome(home), WithLookupEnv(testEnv()))
	stubSQLite(t, nil)
	if !p.Available() {
		t.Fatal("database present: Available should be true without running sqlite3")
	}
}

func TestLoadKeyFallsBackWhenSQLiteMissing(t *testing.T) {
	home := t.TempDir()
	writeAuthFile(t, filepath.Join(home, ".local", "share"), "sk-file-key")
	if err := os.WriteFile(
		filepath.Join(home, ".local", "share", "opencode", "opencode.db"), []byte{}, 0o600,
	); err != nil {
		t.Fatal(err)
	}
	stubSQLite(t, func(string, string) ([]byte, []byte, error) {
		return nil, nil, &exec.Error{Name: "sqlite3", Err: exec.ErrNotFound}
	})
	p := New(WithHome(home), WithLookupEnv(testEnv()))
	key, err := p.loadKey(context.Background())
	if err != nil || key != "sk-file-key" {
		t.Fatalf("missing sqlite3 should fall back to auth.json: got %q, %v", key, err)
	}
}
