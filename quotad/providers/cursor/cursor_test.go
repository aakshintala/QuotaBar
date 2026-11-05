package cursor

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"quotad/monitor"
)

// pipeJWT builds an unsigned JWT with the given payload JSON, the way the
// Swift tests do: base64url without padding.
func pipeJWT(payloadJSON string) string {
	enc := base64.RawURLEncoding.EncodeToString([]byte(payloadJSON))
	return "eyJhbGciOiJIUzI1NiJ9." + enc + ".sig"
}

func testJWT(sub string, exp *float64) string {
	payload := map[string]any{"sub": sub}
	if exp != nil {
		payload["exp"] = *exp
	}
	raw, _ := json.Marshal(payload)
	return "eyJhbGciOiJIUzI1NiJ9." + base64.RawURLEncoding.EncodeToString(raw) + ".sig"
}

func futureExp() *float64 {
	v := float64(time.Now().Add(time.Hour).Unix())
	return &v
}

func pastExp() *float64 {
	v := float64(time.Now().Add(-time.Minute).Unix())
	return &v
}

const probeUsageJSON = `{
	"membershipType": "pro", "isUnlimited": false,
	"individualUsage": {
		"plan": {"enabled": true, "used": 10, "limit": 100},
		"onDemand": {"enabled": false, "used": 0, "limit": null}
	}}`

type stubCall struct {
	name string
	args []string
}

func stubSQLite(t *testing.T, fn func(dbPath, uri, query string) ([]byte, error), calls *[]stubCall) {
	t.Helper()
	old := runCommand
	runCommand = func(_ context.Context, name string, args ...string) ([]byte, error) {
		if calls != nil {
			*calls = append(*calls, stubCall{name, args})
		}
		if len(args) < 2 {
			return nil, errors.New("want uri and query argv")
		}
		return fn("", args[0], args[1])
	}
	t.Cleanup(func() { runCommand = old })
}

// testProvider wires a provider at an httptest server with home pointed at a
// temp dir, so no real credentials are ever touched.
func testProvider(t *testing.T, srv *httptest.Server) (*Provider, string) {
	t.Helper()
	home := t.TempDir()
	p := New(
		WithBaseURL(srv.URL),
		WithHTTPClient(srv.Client()),
		WithHome(home),
		WithXDGConfigHome(filepath.Join(home, ".config")),
	)
	return p, home
}

func writeDB(t *testing.T, home string) string {
	t.Helper()
	path := New(WithHome(home)).dbPath()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte("db"), 0o644); err != nil {
		t.Fatal(err)
	}
	return path
}

func writeAuthJSON(t *testing.T, home, token string) {
	t.Helper()
	path := filepath.Join(home, ".config", "cursor", "auth.json")
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	raw, _ := json.Marshal(map[string]string{"accessToken": token})
	if err := os.WriteFile(path, raw, 0o600); err != nil {
		t.Fatal(err)
	}
}

func TestFetchSuccessSetsCookie(t *testing.T) {
	token := testJWT("github|user_01ABC", futureExp())
	var gotCookie string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotCookie = r.Header.Get("Cookie")
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(probeUsageJSON))
	}))
	defer srv.Close()

	p, home := testProvider(t, srv)
	writeDB(t, home)
	stubSQLite(t, func(_, _ string, _ string) ([]byte, error) {
		return []byte(token + "\n"), nil
	}, nil)

	snap, err := p.Fetch(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if len(snap.Quotas) != 1 || snap.ProviderID != "cursor" {
		t.Errorf("snapshot = %+v", snap)
	}
	want := "WorkosCursorSessionToken=user_01ABC%3A%3A" + token
	if gotCookie != want {
		t.Errorf("cookie = %q, want id after last | with %%3A%%3A", gotCookie)
	}
}

func TestDBTokenWinsOverAuthJSON(t *testing.T) {
	dbToken := testJWT("db|dbuser", futureExp())
	fileToken := testJWT("file|fileuser", futureExp())
	var gotCookie string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotCookie = r.Header.Get("Cookie")
		_, _ = w.Write([]byte(probeUsageJSON))
	}))
	defer srv.Close()

	p, home := testProvider(t, srv)
	writeDB(t, home)
	writeAuthJSON(t, home, fileToken)
	stubSQLite(t, func(_, _ string, _ string) ([]byte, error) {
		return []byte(dbToken), nil
	}, nil)

	if _, err := p.Fetch(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(gotCookie, "WorkosCursorSessionToken=dbuser%3A%3A") {
		t.Errorf("cookie = %q, want the database token first", gotCookie)
	}
}

func TestAuthJSONFallbackWithoutDB(t *testing.T) {
	token := testJWT("fileonly", futureExp())
	var sqliteCalls int
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte(probeUsageJSON))
	}))
	defer srv.Close()

	p, home := testProvider(t, srv)
	writeAuthJSON(t, home, token)
	old := runCommand
	runCommand = func(context.Context, string, ...string) ([]byte, error) {
		sqliteCalls++
		return nil, errors.New("must not run without a database file")
	}
	t.Cleanup(func() { runCommand = old })

	snap, err := p.Fetch(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if len(snap.Quotas) != 1 {
		t.Errorf("quotas = %d", len(snap.Quotas))
	}
	if sqliteCalls != 0 {
		t.Errorf("sqlite ran %d times without a database file", sqliteCalls)
	}
}

func TestSafeStoragePrefixFallsThroughToAuthJSON(t *testing.T) {
	token := testJWT("agentuser", futureExp())
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte(probeUsageJSON))
	}))
	defer srv.Close()

	p, home := testProvider(t, srv)
	writeDB(t, home)
	writeAuthJSON(t, home, token)
	stubSQLite(t, func(_, _ string, _ string) ([]byte, error) {
		return []byte("cursor-safe-storage:v1:eHh4"), nil
	}, nil)

	snap, err := p.Fetch(context.Background())
	if err != nil {
		t.Fatalf("safe-storage value should fall through to auth.json: %v", err)
	}
	if len(snap.Quotas) != 1 {
		t.Errorf("quotas = %d", len(snap.Quotas))
	}
}

func TestSafeStorageWithoutFallbackExplains(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte(probeUsageJSON))
	}))
	defer srv.Close()

	p, home := testProvider(t, srv)
	writeDB(t, home)
	stubSQLite(t, func(_, _ string, _ string) ([]byte, error) {
		return []byte("cursor-safe-storage:v1:eHh4"), nil
	}, nil)

	_, err := p.Fetch(context.Background())
	if err == nil || !strings.Contains(err.Error(), "cursor-safe-storage") {
		t.Errorf("err = %v, want a message naming cursor-safe-storage", err)
	}
}

func TestMissingCredentialsAskForLogin(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte(probeUsageJSON))
	}))
	defer srv.Close()

	p, _ := testProvider(t, srv)
	stubSQLite(t, func(_, _ string, _ string) ([]byte, error) {
		return nil, errors.New("no db")
	}, nil)

	_, err := p.Fetch(context.Background())
	if err == nil || !strings.Contains(strings.ToLower(err.Error()), "log in") {
		t.Errorf("err = %v, want a log-in error", err)
	}
}

func TestUnexpiredTokenReusedWithoutReread(t *testing.T) {
	token := testJWT("cached", futureExp())
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte(probeUsageJSON))
	}))
	defer srv.Close()

	p, home := testProvider(t, srv)
	dbPath := writeDB(t, home)
	var calls int
	stubSQLite(t, func(_, _ string, _ string) ([]byte, error) {
		calls++
		return []byte(token), nil
	}, nil)

	if _, err := p.Fetch(context.Background()); err != nil {
		t.Fatal(err)
	}
	// Remove the database: a second read would now fail, so success proves
	// the cached token was reused.
	if err := os.Remove(dbPath); err != nil {
		t.Fatal(err)
	}
	snap, err := p.Fetch(context.Background())
	if err != nil {
		t.Fatalf("cached token was not reused: %v", err)
	}
	if len(snap.Quotas) != 1 {
		t.Errorf("quotas = %d", len(snap.Quotas))
	}
	if calls != 1 {
		t.Errorf("sqlite ran %d times, want 1", calls)
	}
}

func TestExpiredTokenIsReread(t *testing.T) {
	token := testJWT("stale", pastExp())
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte(probeUsageJSON))
	}))
	defer srv.Close()

	p, home := testProvider(t, srv)
	dbPath := writeDB(t, home)
	stubSQLite(t, func(_, _ string, _ string) ([]byte, error) {
		return []byte(token), nil
	}, nil)

	if _, err := p.Fetch(context.Background()); err != nil {
		t.Fatal(err)
	}
	// The cached entry was already expired, so the next fetch must hit the
	// database again. Deleting it proves the re-read was attempted.
	if err := os.Remove(dbPath); err != nil {
		t.Fatal(err)
	}
	if _, err := p.Fetch(context.Background()); err == nil {
		t.Error("expected an error after the database disappeared")
	}
}

func Test401DropsCacheAndRetriesOnce(t *testing.T) {
	token := testJWT("rotated", futureExp())
	var hits atomic.Int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if hits.Add(1) == 1 {
			w.WriteHeader(http.StatusUnauthorized)
			return
		}
		_, _ = w.Write([]byte(probeUsageJSON))
	}))
	defer srv.Close()

	p, home := testProvider(t, srv)
	writeDB(t, home)
	stubSQLite(t, func(_, _ string, _ string) ([]byte, error) {
		return []byte(token), nil
	}, nil)

	snap, err := p.Fetch(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if len(snap.Quotas) != 1 {
		t.Errorf("quotas = %d", len(snap.Quotas))
	}
	if hits.Load() != 2 {
		t.Errorf("http hits = %d, want 2", hits.Load())
	}
}

func TestPersistent401AsksForLogin(t *testing.T) {
	token := testJWT("dead", futureExp())
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusUnauthorized)
	}))
	defer srv.Close()

	p, home := testProvider(t, srv)
	writeDB(t, home)
	stubSQLite(t, func(_, _ string, _ string) ([]byte, error) {
		return []byte(token), nil
	}, nil)

	_, err := p.Fetch(context.Background())
	if err == nil || !strings.Contains(strings.ToLower(err.Error()), "log in") {
		t.Errorf("err = %v, want a log-in error", err)
	}
}

func TestPersistent403AsksForLogin(t *testing.T) {
	token := testJWT("dead", futureExp())
	var hits atomic.Int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits.Add(1)
		w.WriteHeader(http.StatusForbidden)
	}))
	defer srv.Close()

	p, home := testProvider(t, srv)
	writeDB(t, home)
	stubSQLite(t, func(_, _ string, _ string) ([]byte, error) {
		return []byte(token), nil
	}, nil)

	_, err := p.Fetch(context.Background())
	if err == nil || !strings.Contains(strings.ToLower(err.Error()), "log in") {
		t.Errorf("err = %v, want a log-in error", err)
	}
	if hits.Load() != 2 {
		t.Errorf("http hits = %d, want a single retry", hits.Load())
	}
}

func TestRateLimitedHonoursRetryAfter(t *testing.T) {
	token := testJWT("user", futureExp())
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Retry-After", "120")
		w.WriteHeader(http.StatusTooManyRequests)
	}))
	defer srv.Close()

	p, home := testProvider(t, srv)
	writeDB(t, home)
	stubSQLite(t, func(_, _ string, _ string) ([]byte, error) {
		return []byte(token), nil
	}, nil)

	before := time.Now()
	_, err := p.Fetch(context.Background())
	var throttled *monitor.RateLimitedError
	if !errors.As(err, &throttled) {
		t.Fatalf("err = %v, want RateLimitedError", err)
	}
	if throttled.RetryAt.Before(before.Add(115 * time.Second)) {
		t.Errorf("retryAt = %v, want ~120s out", throttled.RetryAt)
	}
}

func TestSQLiteReadOnlyURIAndBusyTimeout(t *testing.T) {
	token := testJWT("user", futureExp())
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte(probeUsageJSON))
	}))
	defer srv.Close()

	p, home := testProvider(t, srv)
	dbPath := writeDB(t, home)
	var calls []stubCall
	stubSQLite(t, func(_, uri, query string) ([]byte, error) {
		if !strings.HasPrefix(uri, "file:") || !strings.Contains(uri, "mode=ro") {
			t.Errorf("uri = %q, want a read-only file: URI", uri)
		}
		if !strings.Contains(query, "busy_timeout") || !strings.Contains(query, "250") {
			t.Errorf("query = %q, want a 250 ms busy timeout", query)
		}
		if !strings.Contains(query, "cursorAuth/accessToken") {
			t.Errorf("query = %q, want the accessToken select", query)
		}
		return []byte(token), nil
	}, &calls)

	if _, err := p.Fetch(context.Background()); err != nil {
		t.Fatal(err)
	}
	if len(calls) != 1 || calls[0].name != "sqlite3" {
		t.Fatalf("calls = %+v, want one sqlite3 call", calls)
	}
	for _, a := range calls[0].args {
		if strings.Contains(a, token) {
			t.Error("token leaked into sqlite argv")
		}
	}
	_ = dbPath
}

func TestSQLiteImmutableOnlyWithoutSidecars(t *testing.T) {
	token := testJWT("user", futureExp())
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte(probeUsageJSON))
	}))
	defer srv.Close()

	p, home := testProvider(t, srv)
	dbPath := writeDB(t, home)
	var uris []string
	stubSQLite(t, func(_, uri, _ string) ([]byte, error) {
		uris = append(uris, uri)
		return []byte(token), nil
	}, nil)

	if _, err := p.Fetch(context.Background()); err != nil {
		t.Fatal(err)
	}
	if len(uris) != 1 || !strings.Contains(uris[0], "immutable=1") {
		t.Errorf("uris = %v, want immutable=1 with no sidecars", uris)
	}

	// With a live WAL sidecar present, immutable must not be used.
	if err := os.WriteFile(dbPath+"-wal", []byte("wal"), 0o644); err != nil {
		t.Fatal(err)
	}
	// Force a fresh read by clearing the first provider's cache path: use a
	// new provider sharing the same home.
	p3 := New(WithBaseURL(srv.URL), WithHTTPClient(srv.Client()),
		WithHome(home), WithXDGConfigHome(filepath.Join(home, ".config")))
	uris = nil
	old := runCommand
	runCommand = func(_ context.Context, _ string, args ...string) ([]byte, error) {
		uris = append(uris, args[0])
		return []byte(token), nil
	}
	t.Cleanup(func() { runCommand = old })
	if _, err := p3.Fetch(context.Background()); err != nil {
		t.Fatal(err)
	}
	if len(uris) != 1 || strings.Contains(uris[0], "immutable=1") {
		t.Errorf("uris = %v, want no immutable=1 beside a WAL sidecar", uris)
	}
}

func TestAvailableStatsOnly(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {}))
	defer srv.Close()

	p, home := testProvider(t, srv)
	if p.Available() {
		t.Error("available with no credential sources")
	}
	writeAuthJSON(t, home, testJWT("u", futureExp()))
	if !p.Available() {
		t.Error("not available with auth.json present")
	}
}

func TestAvailableWithDatabase(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {}))
	defer srv.Close()

	p, home := testProvider(t, srv)
	writeDB(t, home)
	if !p.Available() {
		t.Error("not available with the database present")
	}
}

func TestLinuxConfigHomeResolution(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {}))
	defer srv.Close()

	home := t.TempDir()
	xdg := filepath.Join(home, "xdg")
	p := New(WithBaseURL(srv.URL), WithHTTPClient(srv.Client()),
		WithHome(home), WithXDGConfigHome(xdg), WithGOOS("linux"))
	want := filepath.Join(xdg, "Cursor", "User", "globalStorage", "state.vscdb")
	if p.dbPath() != want {
		t.Errorf("db = %q, want %q", p.dbPath(), want)
	}
	if err := os.MkdirAll(filepath.Dir(want), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(want, []byte("db"), 0o644); err != nil {
		t.Fatal(err)
	}
	if !p.Available() {
		t.Error("not available with the Linux database present")
	}
}

func TestProviderID(t *testing.T) {
	if New().ID() != "cursor" {
		t.Errorf("id = %q", New().ID())
	}
}
