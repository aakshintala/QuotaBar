package claude

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"quotad/monitor"
)

// writeCreds writes a ~/.claude/.credentials.json record under home.
func writeCreds(t *testing.T, home, access, refresh string, expiresAt *float64, subscription string) {
	t.Helper()
	oauth := map[string]any{"accessToken": access}
	if refresh != "" {
		oauth["refreshToken"] = refresh
	}
	if expiresAt != nil {
		oauth["expiresAt"] = *expiresAt
	}
	if subscription != "" {
		oauth["subscriptionType"] = subscription
	}
	data, err := json.Marshal(map[string]any{"claudeAiOauth": oauth})
	if err != nil {
		t.Fatal(err)
	}
	dir := filepath.Join(home, ".claude")
	if err := os.MkdirAll(dir, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, ".credentials.json"), data, 0o600); err != nil {
		t.Fatal(err)
	}
}

func futureMs(d time.Duration) *float64 {
	ms := float64(time.Now().Add(d).UnixMilli())
	return &ms
}

func pastMs(d time.Duration) *float64 {
	ms := float64(time.Now().Add(-d).UnixMilli())
	return &ms
}

// stubServer serves usage and refresh endpoints with hit counters.
type stubServer struct {
	server     *httptest.Server
	usageHits  atomic.Int64
	refreshHit atomic.Int64
	usage      func(w http.ResponseWriter, r *http.Request)
	refresh    func(w http.ResponseWriter, r *http.Request)
}

func newStub(t *testing.T) *stubServer {
	t.Helper()
	s := &stubServer{}
	mux := http.NewServeMux()
	mux.HandleFunc("/api/oauth/usage", func(w http.ResponseWriter, r *http.Request) {
		s.usageHits.Add(1)
		s.usage(w, r)
	})
	mux.HandleFunc("/v1/oauth/token", func(w http.ResponseWriter, r *http.Request) {
		s.refreshHit.Add(1)
		s.refresh(w, r)
	})
	s.server = httptest.NewServer(mux)
	t.Cleanup(s.server.Close)
	return s
}

func testProvider(home string, s *stubServer, now *time.Time) *Provider {
	opts := []Option{
		WithHome(home),
		WithUsageURL(s.server.URL + "/api/oauth/usage"),
		WithRefreshURL(s.server.URL + "/v1/oauth/token"),
		WithUseKeychain(false),
	}
	if now != nil {
		opts = append(opts, WithNow(func() time.Time { return *now }))
	}
	return New(opts...)
}

func quotaByKey(quotas []monitor.Quota, key string) *monitor.Quota {
	for i, q := range quotas {
		if q.Type.Key() == key {
			return &quotas[i]
		}
	}
	return nil
}

func TestParseCapturedFixture(t *testing.T) {
	data, err := os.ReadFile("testdata/usage.json")
	if err != nil {
		t.Fatal(err)
	}
	quotas, tier, err := parseUsage(data, "claude_max")
	if err != nil {
		t.Fatal(err)
	}
	// Session/weekly come only from the legacy windows: the unscoped
	// limits entries must not duplicate them.
	if len(quotas) != 4 {
		t.Fatalf("quotas = %d, want 4 (session, weekly, sonnet, fable)", len(quotas))
	}
	session := quotaByKey(quotas, "session")
	if session == nil || *session.PercentRemaining != 76.5 {
		t.Errorf("session = %+v, want 76.5 remaining", session)
	}
	if session.ResetsAt == nil {
		t.Error("session resetsAt = nil")
	}
	weekly := quotaByKey(quotas, "weekly")
	if weekly == nil || *weekly.PercentRemaining != 59 {
		t.Errorf("weekly = %+v, want 59 remaining", weekly)
	}
	sonnet := quotaByKey(quotas, "model:sonnet")
	if sonnet == nil || *sonnet.PercentRemaining != 88 {
		t.Errorf("sonnet = %+v, want 88 remaining", sonnet)
	}
	if quotaByKey(quotas, "model:opus") != nil {
		t.Error("null seven_day_opus must produce no quota")
	}
	fable := quotaByKey(quotas, "model:fable")
	if fable == nil || *fable.PercentRemaining != 83 {
		t.Errorf("fable = %+v, want 83 remaining", fable)
	}
	if tier == nil || *tier != monitor.ClaudeMaxTier() {
		t.Errorf("tier = %+v, want ULTRA", tier)
	}
}

func TestParseScopedLimitsEdges(t *testing.T) {
	data := `{"five_hour": {"utilization": 10.0, "resets_at": "2025-01-15T10:00:00Z"},
		"seven_day_opus": {"utilization": 60.0, "resets_at": "2025-01-20T00:00:00Z"},
		"limits": [
			{"kind": "weekly_scoped", "percent": 50, "scope": null},
			{"kind": "weekly_scoped", "percent": 50, "scope": {"model": null}},
			{"kind": "weekly_scoped", "percent": 50, "scope": {"model": {"display_name": ""}}},
			{"kind": "weekly_scoped", "scope": {"model": {"display_name": "Opus"}}},
			{"kind": "weekly_scoped", "percent": 105, "scope": {"model": {"display_name": "Fable 5"}}},
			{"kind": "weekly_scoped", "percent": 40, "scope": {"model": {"display_name": "Fable"}}},
			{"kind": "weekly_scoped", "percent": 60, "scope": {"model": {"display_name": "Opus"}}}
		]}`
	quotas, _, err := parseUsage([]byte(data), "pro")
	if err != nil {
		t.Fatal(err)
	}
	if len(quotas) != 3 {
		t.Fatalf("quotas = %d, want 3 (session, opus, fable)", len(quotas))
	}
	fable := quotaByKey(quotas, "model:fable")
	if fable == nil || *fable.PercentRemaining != -5 {
		t.Errorf("fable = %+v, want -5 (first entry wins, over-quota stays negative)", fable)
	}
	opus := quotaByKey(quotas, "model:opus")
	if opus == nil || *opus.PercentRemaining != 40 {
		t.Errorf("opus = %+v, want 40 (legacy field wins over limits array)", opus)
	}
}

func TestParseFractionalResets(t *testing.T) {
	quotas, _, err := parseUsage([]byte(`{"five_hour": {"utilization": 10.0, "resets_at": "2025-01-15T10:00:00.123Z"}}`), "pro")
	if err != nil {
		t.Fatal(err)
	}
	q := quotaByKey(quotas, "session")
	if q == nil || q.ResetsAt == nil {
		t.Fatalf("session = %+v, want a parsed fractional reset", q)
	}
	if got := q.ResetsAt.Nanosecond(); got != 123000000 {
		t.Errorf("reset nanoseconds = %d, want 123000000", got)
	}
}

func TestParseEmptyAndInvalid(t *testing.T) {
	quotas, tier, err := parseUsage([]byte(`{}`), "")
	if err != nil {
		t.Fatal(err)
	}
	if len(quotas) != 0 {
		t.Errorf("quotas = %d, want 0", len(quotas))
	}
	if tier != nil {
		t.Errorf("tier = %+v, want nil", tier)
	}
	if _, _, err := parseUsage([]byte(`not json`), "max"); err == nil {
		t.Error("expected a parse error")
	}
}

func TestTierMapping(t *testing.T) {
	for _, tc := range []struct {
		in   string
		want monitor.Tier
	}{
		{"claude_max", monitor.ClaudeMaxTier()},
		{"max", monitor.ClaudeMaxTier()},
		{"claude_pro", monitor.ClaudeProTier()},
		{"pro", monitor.ClaudeProTier()},
		{"api", monitor.ClaudeAPITier()},
		{"claude_api", monitor.ClaudeAPITier()},
		{"team", monitor.CustomTier("team")},
	} {
		got := tierFor(tc.in)
		if got == nil || *got != tc.want {
			t.Errorf("tierFor(%q) = %+v, want %+v", tc.in, got, tc.want)
		}
	}
}

func TestParseRetryAfter(t *testing.T) {
	now := time.Date(2023, 11, 14, 22, 13, 20, 0, time.UTC)
	for _, tc := range []struct {
		in     string
		want   time.Duration
		wantOK bool
	}{
		{"120", 120 * time.Second, true},
		{"1", time.Second, true},
		{"0", 0, false},
		{"-5", 0, false},
		{"", 0, false},
		{"   ", 0, false},
		{"not a number", 0, false},
		{"Tue, 14 Nov 2023 22:14:20 GMT", time.Minute, true},
		{"Tue, 14 Nov 2023 22:00:00 GMT", 0, false},
	} {
		got, ok := parseRetryAfter(tc.in, now)
		if ok != tc.wantOK || (ok && got != tc.want) {
			t.Errorf("parseRetryAfter(%q) = %v, %v; want %v, %v", tc.in, got, ok, tc.want, tc.wantOK)
		}
	}
}

func TestNeedsRefresh(t *testing.T) {
	now := time.Now()
	if !needsRefresh(&credentials{}, now) {
		t.Error("missing expiry must need a refresh")
	}
	if !needsRefresh(&credentials{expiresAt: futureMs(4 * time.Minute)}, now) {
		t.Error("expiry within the 5 minute buffer must need a refresh")
	}
	if needsRefresh(&credentials{expiresAt: futureMs(time.Hour)}, now) {
		t.Error("expiry an hour out must not need a refresh")
	}
}

func TestAvailable(t *testing.T) {
	home := t.TempDir()
	p := New(WithHome(home), WithUseKeychain(false))
	if p.Available() {
		t.Error("Available with no credentials file")
	}
	writeCreds(t, home, "fake-access", "fake-refresh", futureMs(time.Hour), "pro")
	if !p.Available() {
		t.Error("Available should be true with a credentials file")
	}
}

func TestFetchSuccessAndSnapshotCache(t *testing.T) {
	home := t.TempDir()
	writeCreds(t, home, "fake-access", "fake-refresh", futureMs(time.Hour), "claude_pro")
	s := newStub(t)
	s.usage = func(w http.ResponseWriter, r *http.Request) {
		if got := r.URL.Query().Get("cedar_ember"); got != "1" {
			t.Errorf("missing ?cedar_ember=1, query = %q", r.URL.RawQuery)
		}
		if got := r.Header.Get("Authorization"); got != "Bearer fake-access" {
			t.Errorf("Authorization = %q", got)
		}
		if got := r.Header.Get("User-Agent"); got != "claude-cli/2.1.0 (external, cli)" {
			t.Errorf("User-Agent = %q", got)
		}
		if got := r.Header.Get("anthropic-beta"); got != "oauth-2025-04-20" {
			t.Errorf("anthropic-beta = %q", got)
		}
		w.Write([]byte(`{"five_hour": {"utilization": 25.5, "resets_at": "2025-01-15T10:00:00Z"}}`))
	}
	s.refresh = func(w http.ResponseWriter, r *http.Request) {
		t.Error("fresh token must not trigger a refresh")
	}
	p := testProvider(home, s, nil)
	snap, err := p.Fetch(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if snap.ProviderID != "claude" {
		t.Errorf("provider = %q", snap.ProviderID)
	}
	q := quotaByKey(snap.Quotas, "session")
	if q == nil || *q.PercentRemaining != 74.5 {
		t.Errorf("session = %+v, want 74.5", q)
	}
	if snap.Tier == nil || *snap.Tier != monitor.ClaudeProTier() {
		t.Errorf("tier = %+v, want PRO", snap.Tier)
	}
	// Second fetch is served from the 300 s snapshot cache: no new HTTP.
	if _, err := p.Fetch(context.Background()); err != nil {
		t.Fatal(err)
	}
	if got := s.usageHits.Load(); got != 1 {
		t.Errorf("usage hits = %d, want 1 (snapshot cache)", got)
	}
}

func TestFetchNoCredentials(t *testing.T) {
	s := newStub(t)
	s.usage = func(w http.ResponseWriter, r *http.Request) { t.Error("no request expected") }
	p := testProvider(t.TempDir(), s, nil)
	_, err := p.Fetch(context.Background())
	if err == nil || !strings.Contains(err.Error(), "log in to Claude Code again") {
		t.Errorf("err = %v, want a log-in-again error", err)
	}
}

func TestFetchRefreshesExpiredToken(t *testing.T) {
	home := t.TempDir()
	writeCreds(t, home, "old-access", "old-refresh", pastMs(time.Hour), "max")
	s := newStub(t)
	s.usage = func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer new-access" {
			t.Errorf("usage used %q, want the refreshed token", r.Header.Get("Authorization"))
		}
		w.Write([]byte(`{"five_hour": {"utilization": 10.0}}`))
	}
	s.refresh = func(w http.ResponseWriter, r *http.Request) {
		if ct := r.Header.Get("Content-Type"); ct != "application/x-www-form-urlencoded" {
			t.Errorf("Content-Type = %q", ct)
		}
		if err := r.ParseForm(); err != nil {
			t.Fatal(err)
		}
		if r.Form.Get("grant_type") != "refresh_token" || r.Form.Get("refresh_token") != "old-refresh" || r.Form.Get("client_id") == "" {
			t.Errorf("refresh form = %v", r.Form)
		}
		for k := range r.Form {
			if k == "scope" {
				t.Error("form body must not carry scopes")
			}
		}
		w.Write([]byte(`{"access_token": "new-access", "refresh_token": "new-refresh", "expires_in": 3600}`))
	}
	p := testProvider(home, s, nil)
	snap, err := p.Fetch(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if q := quotaByKey(snap.Quotas, "session"); q == nil || *q.PercentRemaining != 90 {
		t.Errorf("session = %+v, want 90", q)
	}
	// The refreshed token is written back to the file, keeping other fields.
	raw, _ := os.ReadFile(filepath.Join(home, ".claude", ".credentials.json"))
	var decoded map[string]any
	if err := json.Unmarshal(raw, &decoded); err != nil {
		t.Fatal(err)
	}
	oauth := decoded["claudeAiOauth"].(map[string]any)
	if oauth["accessToken"] != "new-access" || oauth["refreshToken"] != "new-refresh" {
		t.Errorf("written oauth = %v", oauth)
	}
	if oauth["subscriptionType"] != "max" {
		t.Errorf("subscriptionType lost on writeback: %v", oauth)
	}
	if _, err := os.Stat(filepath.Join(home, ".claude", ".credentials.json")); err != nil {
		t.Fatal(err)
	}
}

func TestFetchWritebackPreservesUnknownFields(t *testing.T) {
	home := t.TempDir()
	dir := filepath.Join(home, ".claude")
	if err := os.MkdirAll(dir, 0o700); err != nil {
		t.Fatal(err)
	}
	record := map[string]any{
		"claudeAiOauth": map[string]any{
			"accessToken": "old", "refreshToken": "r", "expiresAt": float64(*pastMs(time.Hour)),
			"subscriptionType": "pro", "scopes": []any{"a"}, "rateLimitTier": "t1",
		},
		"otherTop": "keep",
	}
	data, _ := json.Marshal(record)
	if err := os.WriteFile(filepath.Join(dir, ".credentials.json"), data, 0o600); err != nil {
		t.Fatal(err)
	}
	s := newStub(t)
	s.usage = func(w http.ResponseWriter, r *http.Request) { w.Write([]byte(`{}`)) }
	s.refresh = func(w http.ResponseWriter, r *http.Request) {
		w.Write([]byte(`{"access_token": "new", "expires_in": 3600}`))
	}
	if _, err := testProvider(home, s, nil).Fetch(context.Background()); err != nil {
		t.Fatal(err)
	}
	raw, _ := os.ReadFile(filepath.Join(dir, ".credentials.json"))
	var decoded map[string]any
	if err := json.Unmarshal(raw, &decoded); err != nil {
		t.Fatal(err)
	}
	if decoded["otherTop"] != "keep" {
		t.Errorf("top-level field lost: %v", decoded)
	}
	oauth := decoded["claudeAiOauth"].(map[string]any)
	if oauth["accessToken"] != "new" || oauth["refreshToken"] != "r" {
		t.Errorf("tokens wrong after writeback: %v", oauth)
	}
	if scopes, _ := oauth["scopes"].([]any); len(scopes) != 1 || scopes[0] != "a" {
		t.Errorf("nested scopes lost: %v", oauth)
	}
	if oauth["rateLimitTier"] != "t1" {
		t.Errorf("nested rateLimitTier lost: %v", oauth)
	}
}

func TestFetchInvalidGrantStopsRefreshing(t *testing.T) {
	home := t.TempDir()
	writeCreds(t, home, "old-access", "dead-refresh", pastMs(time.Hour), "pro")
	s := newStub(t)
	s.usage = func(w http.ResponseWriter, r *http.Request) { w.Write([]byte(`{}`)) }
	s.refresh = func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusBadRequest)
		w.Write([]byte(`{"error": "invalid_grant", "error_description": "revoked"}`))
	}
	now := time.Now()
	p := testProvider(home, s, &now)
	_, err := p.Fetch(context.Background())
	if err == nil || !strings.Contains(err.Error(), "log in to Claude Code again") {
		t.Fatalf("err = %v, want a log-in-again error", err)
	}
	if _, err := p.Fetch(context.Background()); err == nil {
		t.Fatal("expected an error on the second fetch")
	}
	// The revoked token is never redeemed again.
	if got := s.refreshHit.Load(); got != 1 {
		t.Errorf("refresh hits = %d, want 1 (terminal invalid_grant)", got)
	}
}

func TestFetchRefreshBackoff(t *testing.T) {
	home := t.TempDir()
	writeCreds(t, home, "old-access", "flaky-refresh", pastMs(time.Hour), "pro")
	s := newStub(t)
	s.usage = func(w http.ResponseWriter, r *http.Request) { w.Write([]byte(`{}`)) }
	s.refresh = func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusInternalServerError)
		w.Write([]byte(`boom`))
	}
	now := time.Now()
	p := testProvider(home, s, &now)
	if _, err := p.Fetch(context.Background()); err == nil {
		t.Fatal("expected a refresh error")
	}
	// 5 minute backoff: no second attempt yet.
	now = now.Add(4 * time.Minute)
	if _, err := p.Fetch(context.Background()); err == nil {
		t.Fatal("expected a backoff error")
	}
	if got := s.refreshHit.Load(); got != 1 {
		t.Fatalf("refresh hits = %d, want 1 (backing off)", got)
	}
	// After the window, it retries, then backs off doubly (10 min).
	now = now.Add(2 * time.Minute)
	if _, err := p.Fetch(context.Background()); err == nil {
		t.Fatal("expected a refresh error")
	}
	if got := s.refreshHit.Load(); got != 2 {
		t.Fatalf("refresh hits = %d, want 2", got)
	}
	now = now.Add(9 * time.Minute)
	if _, err := p.Fetch(context.Background()); err == nil {
		t.Fatal("expected a backoff error")
	}
	if got := s.refreshHit.Load(); got != 2 {
		t.Errorf("refresh hits = %d, want 2 (doubled backoff)", got)
	}
}

func TestFetchPicksUpRotatedCredentials(t *testing.T) {
	home := t.TempDir()
	writeCreds(t, home, "stale-access", "stale-refresh", pastMs(time.Hour), "pro")
	s := newStub(t)
	s.usage = func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer cli-access" {
			t.Errorf("usage auth = %q, want the CLI-rotated token", r.Header.Get("Authorization"))
		}
		w.Write([]byte(`{"five_hour": {"utilization": 20.0}}`))
	}
	s.refresh = func(w http.ResponseWriter, r *http.Request) {
		// The CLI rotated the file between load and refresh: the stale
		// redeem fails, the reloaded one is fresh and needs no redeem.
		if r.FormValue("refresh_token") == "stale-refresh" {
			writeCreds(t, home, "cli-access", "cli-refresh", futureMs(time.Hour), "pro")
			w.WriteHeader(http.StatusBadRequest)
			w.Write([]byte(`{"error": "temporarily_unavailable"}`))
			return
		}
		t.Errorf("unexpected refresh for %q", r.FormValue("refresh_token"))
	}
	p := testProvider(home, s, nil)
	snap, err := p.Fetch(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if q := quotaByKey(snap.Quotas, "session"); q == nil || *q.PercentRemaining != 80 {
		t.Errorf("session = %+v, want 80", q)
	}
	if got := s.refreshHit.Load(); got != 1 {
		t.Errorf("refresh hits = %d, want 1", got)
	}
}

func TestFetch401RefreshesOnce(t *testing.T) {
	home := t.TempDir()
	writeCreds(t, home, "weak-access", "good-refresh", futureMs(time.Hour), "pro")
	s := newStub(t)
	s.usage = func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") == "Bearer strong-access" {
			w.Write([]byte(`{"five_hour": {"utilization": 10.0}}`))
			return
		}
		w.WriteHeader(http.StatusUnauthorized)
	}
	s.refresh = func(w http.ResponseWriter, r *http.Request) {
		w.Write([]byte(`{"access_token": "strong-access", "refresh_token": "good-refresh", "expires_in": 3600}`))
	}
	snap, err := testProvider(home, s, nil).Fetch(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if q := quotaByKey(snap.Quotas, "session"); q == nil || *q.PercentRemaining != 90 {
		t.Errorf("session = %+v, want 90", q)
	}
	if got := s.refreshHit.Load(); got != 1 {
		t.Errorf("refresh hits = %d, want 1", got)
	}
}

func TestFetchRateLimitPersistsAcrossRestart(t *testing.T) {
	home := t.TempDir()
	writeCreds(t, home, "limited-access", "fake-refresh", futureMs(time.Hour), "pro")
	s := newStub(t)
	s.usage = func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Retry-After", "120")
		w.WriteHeader(http.StatusTooManyRequests)
	}
	now := time.Now()
	p := testProvider(home, s, &now)
	_, err := p.Fetch(context.Background())
	var limited *monitor.RateLimitedError
	if !errors.As(err, &limited) {
		t.Fatalf("err = %v, want *RateLimitedError", err)
	}
	if dt := limited.RetryAt.Sub(now); dt < 119*time.Second || dt > 121*time.Second {
		t.Errorf("retry in %v, want ~120s", dt)
	}
	raw, ferr := os.ReadFile(filepath.Join(home, ".quotabar", "claude", "ratelimit.json"))
	if ferr != nil {
		t.Fatalf("rate-limit file missing: %v", ferr)
	}
	if strings.Contains(string(raw), "limited-access") {
		t.Error("rate-limit file must not contain the token")
	}
	info, _ := os.Stat(filepath.Join(home, ".quotabar", "claude", "ratelimit.json"))
	if perm := info.Mode().Perm(); perm != 0o600 {
		t.Errorf("rate-limit file mode = %o, want 600", perm)
	}
	// A fresh provider (simulated restart) honors the persisted gate
	// without touching the network.
	before := s.usageHits.Load()
	p2 := testProvider(home, s, &now)
	if _, err := p2.Fetch(context.Background()); !errors.As(err, &limited) {
		t.Errorf("restarted err = %v, want *RateLimitedError", err)
	}
	if got := s.usageHits.Load(); got != before {
		t.Errorf("usage hits grew %d -> %d while gated", before, got)
	}
	// After the window the fetch succeeds and the gate is cleared.
	now = now.Add(3 * time.Minute)
	s.usage = func(w http.ResponseWriter, r *http.Request) { w.Write([]byte(`{}`)) }
	if _, err := p2.Fetch(context.Background()); err != nil {
		t.Fatal(err)
	}
	if stored := p2.readRateFile(); len(stored) != 0 {
		t.Errorf("gate not cleared after success: %v", stored)
	}
}

func TestRestartInsideCooldownServesPersistedSnapshot(t *testing.T) {
	home := t.TempDir()
	writeCreds(t, home, "limited-access", "fake-refresh", futureMs(24*time.Hour), "pro")
	s := newStub(t)
	now := time.Now()
	sessionReset := now.Add(24 * time.Hour).UTC().Format(time.RFC3339)
	weeklyReset := now.Add(7 * 24 * time.Hour).UTC().Format(time.RFC3339)
	s.usage = func(w http.ResponseWriter, r *http.Request) {
		w.Write([]byte(usageJSON(25.5, 41.0, sessionReset, weeklyReset)))
	}
	p := testProvider(home, s, &now)
	first, err := p.Fetch(context.Background())
	if err != nil {
		t.Fatalf("first Fetch: %v", err)
	}
	// The endpoint starts 429ing. This provider holds the reading in
	// memory, so it still reports throttled.
	s.usage = func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Retry-After", "3600")
		w.WriteHeader(http.StatusTooManyRequests)
	}
	now = now.Add(301 * time.Second) // past the 300 s snapshot cache
	var limited *monitor.RateLimitedError
	if _, err := p.Fetch(context.Background()); !errors.As(err, &limited) {
		t.Fatalf("gated err = %v, want *RateLimitedError", err)
	}
	// A fresh provider (simulated restart) inside the cooldown serves the
	// persisted snapshot with its original CapturedAt, without HTTP.
	before := s.usageHits.Load()
	p2 := testProvider(home, s, &now)
	snap, err := p2.Fetch(context.Background())
	if err != nil {
		t.Fatalf("restart Fetch: %v", err)
	}
	if !snap.CapturedAt.Equal(first.CapturedAt) {
		t.Errorf("CapturedAt = %v, want %v", snap.CapturedAt, first.CapturedAt)
	}
	session := quotaByKey(snap.Quotas, "session")
	if session == nil || *session.PercentRemaining != 74.5 {
		t.Errorf("session = %+v, want 74.5 remaining", session)
	}
	weekly := quotaByKey(snap.Quotas, "weekly")
	if weekly == nil || *weekly.PercentRemaining != 59 {
		t.Errorf("weekly = %+v, want 59 remaining", weekly)
	}
	if snap.Tier == nil || *snap.Tier != monitor.ClaudeProTier() {
		t.Errorf("tier = %+v, want PRO", snap.Tier)
	}
	if got := s.usageHits.Load(); got != before {
		t.Errorf("usage hits grew %d -> %d while gated", before, got)
	}
}

func TestRestartWithOtherAccountSnapshotIsIgnored(t *testing.T) {
	home := t.TempDir()
	writeCreds(t, home, "limited-access", "fake-refresh", futureMs(24*time.Hour), "pro")
	s := newStub(t)
	now := time.Now()
	s.usage = func(w http.ResponseWriter, r *http.Request) {
		w.Write([]byte(usageJSON(25.5, 41.0,
			now.Add(24*time.Hour).UTC().Format(time.RFC3339),
			now.Add(7*24*time.Hour).UTC().Format(time.RFC3339))))
	}
	p := testProvider(home, s, &now)
	if _, err := p.Fetch(context.Background()); err != nil {
		t.Fatalf("first Fetch: %v", err)
	}
	if _, err := os.Stat(filepath.Join(home, ".quotabar", "claude", "snapshot.json")); err != nil {
		t.Fatalf("snapshot file missing after success: %v", err)
	}
	// A different login hits its own 429. Its gate must not serve the
	// other account's persisted snapshot.
	writeCreds(t, home, "other-access", "other-refresh", futureMs(24*time.Hour), "pro")
	s.usage = func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Retry-After", "3600")
		w.WriteHeader(http.StatusTooManyRequests)
	}
	now = now.Add(301 * time.Second)
	if _, err := p.Fetch(context.Background()); err == nil {
		t.Fatal("expected a throttled error")
	}
	var limited *monitor.RateLimitedError
	if _, err := testProvider(home, s, &now).Fetch(context.Background()); !errors.As(err, &limited) {
		t.Errorf("restarted err = %v, want *RateLimitedError", err)
	}
}

func TestRestartWithExpiredSnapshotServesRateLimited(t *testing.T) {
	home := t.TempDir()
	writeCreds(t, home, "limited-access", "fake-refresh", futureMs(24*time.Hour), "pro")
	s := newStub(t)
	now := time.Now()
	s.usage = func(w http.ResponseWriter, r *http.Request) {
		w.Write([]byte(usageJSON(25.5, 41.0,
			now.Add(10*time.Minute).UTC().Format(time.RFC3339),
			now.Add(20*time.Minute).UTC().Format(time.RFC3339))))
	}
	p := testProvider(home, s, &now)
	if _, err := p.Fetch(context.Background()); err != nil {
		t.Fatalf("first Fetch: %v", err)
	}
	s.usage = func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Retry-After", "7200")
		w.WriteHeader(http.StatusTooManyRequests)
	}
	now = now.Add(301 * time.Second)
	if _, err := p.Fetch(context.Background()); err == nil {
		t.Fatal("expected a throttled error")
	}
	// Past every persisted reset but still inside the gate: nothing
	// unexpired remains, so the restart stays throttled.
	now = now.Add(time.Hour)
	var limited *monitor.RateLimitedError
	if _, err := testProvider(home, s, &now).Fetch(context.Background()); !errors.As(err, &limited) {
		t.Errorf("restarted err = %v, want *RateLimitedError", err)
	}
}

func TestGatedFetchIgnoresCorruptSnapshot(t *testing.T) {
	home := t.TempDir()
	writeCreds(t, home, "limited-access", "fake-refresh", futureMs(time.Hour), "pro")
	s := newStub(t)
	s.usage = func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Retry-After", "3600")
		w.WriteHeader(http.StatusTooManyRequests)
	}
	now := time.Now()
	var limited *monitor.RateLimitedError
	if _, err := testProvider(home, s, &now).Fetch(context.Background()); !errors.As(err, &limited) {
		t.Fatalf("err = %v, want *RateLimitedError", err)
	}
	dir := filepath.Join(home, ".quotabar", "claude")
	if err := os.MkdirAll(dir, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "snapshot.json"), []byte("not json{"), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := testProvider(home, s, &now).Fetch(context.Background()); !errors.As(err, &limited) {
		t.Errorf("restarted err = %v, want *RateLimitedError", err)
	}
}

func TestSuccessPersistsSnapshotFile(t *testing.T) {
	home := t.TempDir()
	writeCreds(t, home, "persist-access", "persist-refresh", futureMs(time.Hour), "pro")
	s := newStub(t)
	now := time.Now()
	s.usage = func(w http.ResponseWriter, r *http.Request) {
		w.Write([]byte(usageJSON(25.5, 41.0,
			now.Add(24*time.Hour).UTC().Format(time.RFC3339),
			now.Add(7*24*time.Hour).UTC().Format(time.RFC3339))))
	}
	if _, err := testProvider(home, s, &now).Fetch(context.Background()); err != nil {
		t.Fatal(err)
	}
	if info, err := os.Stat(filepath.Join(home, ".quotabar", "claude")); err != nil {
		t.Fatal(err)
	} else if perm := info.Mode().Perm(); perm != 0o700 {
		t.Errorf("snapshot dir mode = %o, want 700", perm)
	}
	path := filepath.Join(home, ".quotabar", "claude", "snapshot.json")
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("snapshot file missing: %v", err)
	}
	if info, err := os.Stat(path); err != nil {
		t.Fatal(err)
	} else if perm := info.Mode().Perm(); perm != 0o600 {
		t.Errorf("snapshot file mode = %o, want 600", perm)
	}
	for _, secret := range []string{"persist-access", "persist-refresh"} {
		if strings.Contains(string(raw), secret) {
			t.Errorf("snapshot file contains %q", secret)
		}
	}
	for _, want := range []string{`"providerId":"claude"`, "74.5", "59", `"Badge":"PRO"`} {
		if !strings.Contains(string(raw), want) {
			t.Errorf("snapshot file missing %s: %s", want, raw)
		}
	}
}

func usageJSON(sessionUtil, weeklyUtil float64, sessionReset, weeklyReset string) string {
	return `{"five_hour": {"utilization": ` + strconv.FormatFloat(sessionUtil, 'f', 1, 64) +
		`, "resets_at": "` + sessionReset + `"}, "seven_day": {"utilization": ` +
		strconv.FormatFloat(weeklyUtil, 'f', 1, 64) + `, "resets_at": "` + weeklyReset + `"}}`
}

func TestFetchRateLimitDefaultWindow(t *testing.T) {
	home := t.TempDir()
	writeCreds(t, home, "limited-access", "fake-refresh", futureMs(time.Hour), "pro")
	s := newStub(t)
	s.usage = func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusTooManyRequests)
	}
	now := time.Now()
	p := testProvider(home, s, &now)
	_, err := p.Fetch(context.Background())
	var limited *monitor.RateLimitedError
	if !errors.As(err, &limited) {
		t.Fatalf("err = %v, want *RateLimitedError", err)
	}
	if dt := limited.RetryAt.Sub(now); dt < 299*time.Second || dt > 301*time.Second {
		t.Errorf("retry in %v, want ~5m default", dt)
	}
}

func TestFetchKeychainFallback(t *testing.T) {
	home := t.TempDir()
	record := `{"claudeAiOauth": {"accessToken": "kc-access", "refreshToken": "kc-refresh", "expiresAt": 9999999999999, "subscriptionType": "api", "keep": 1}}`
	oldRead, oldWrite := keychainRead, keychainWrite
	var written []byte
	var writeAccount string
	keychainRead = func(service, account string) ([]byte, error) {
		if service != "Claude Code-credentials" || account != "tester" {
			t.Errorf("read(%q, %q)", service, account)
		}
		return []byte(record), nil
	}
	keychainWrite = func(service, account string, data []byte) error {
		writeAccount = account
		written = data
		return nil
	}
	t.Cleanup(func() { keychainRead, keychainWrite = oldRead, oldWrite })
	s := newStub(t)
	s.usage = func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer kc-access" {
			t.Errorf("usage auth = %q", r.Header.Get("Authorization"))
		}
		w.Write([]byte(`{}`))
	}
	s.refresh = func(w http.ResponseWriter, r *http.Request) {
		t.Error("fresh Keychain token must not trigger a refresh")
	}
	p := New(WithHome(home), WithUsageURL(s.server.URL+"/api/oauth/usage"),
		WithRefreshURL(s.server.URL+"/v1/oauth/token"),
		WithUseKeychain(true), WithKeychainAccount("tester"))
	snap, err := p.Fetch(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if snap.Tier == nil || *snap.Tier != monitor.ClaudeAPITier() {
		t.Errorf("tier = %+v, want API", snap.Tier)
	}
	// No file fallback happened: the Keychain was the source.
	if _, err := os.Stat(filepath.Join(home, ".claude", ".credentials.json")); !os.IsNotExist(err) {
		t.Error("file must not be created for a Keychain source")
	}
	if written != nil || writeAccount != "" {
		t.Error("fresh token must not trigger a writeback")
	}
}

func TestSaveToKeychainOnRefresh(t *testing.T) {
	home := t.TempDir()
	oldRead, oldWrite := keychainRead, keychainWrite
	expired := `{"claudeAiOauth": {"accessToken": "old", "refreshToken": "r", "expiresAt": 1000, "keep": "yes"}}`
	var written []byte
	var gotAccount, gotService string
	keychainRead = func(service, account string) ([]byte, error) { return []byte(expired), nil }
	keychainWrite = func(service, account string, data []byte) error {
		gotService, gotAccount, written = service, account, data
		return nil
	}
	t.Cleanup(func() { keychainRead, keychainWrite = oldRead, oldWrite })
	s := newStub(t)
	s.usage = func(w http.ResponseWriter, r *http.Request) { w.Write([]byte(`{}`)) }
	s.refresh = func(w http.ResponseWriter, r *http.Request) {
		w.Write([]byte(`{"access_token": "fresh", "expires_in": 60}`))
	}
	p := New(WithHome(home), WithUsageURL(s.server.URL+"/api/oauth/usage"),
		WithRefreshURL(s.server.URL+"/v1/oauth/token"),
		WithUseKeychain(true), WithKeychainAccount("tester"))
	if _, err := p.Fetch(context.Background()); err != nil {
		t.Fatal(err)
	}
	if gotService != "Claude Code-credentials" || gotAccount != "tester" {
		t.Errorf("write(%q, %q)", gotService, gotAccount)
	}
	var decoded map[string]any
	if err := json.Unmarshal(written, &decoded); err != nil {
		t.Fatal(err)
	}
	oauth := decoded["claudeAiOauth"].(map[string]any)
	if oauth["accessToken"] != "fresh" || oauth["keep"] != "yes" {
		t.Errorf("keychain writeback = %v", oauth)
	}
	if strings.Contains(string(written), `"refreshToken"`) && oauth["refreshToken"] != "r" {
		t.Errorf("refresh token changed: %v", oauth)
	}
}

func TestAvailableKeychainFallbackWithoutProcess(t *testing.T) {
	home := t.TempDir()
	oldRead := keychainRead
	keychainRead = func(service, account string) ([]byte, error) {
		t.Error("Available must not invoke a Keychain process call")
		return nil, errors.New("no keychain in Available")
	}
	t.Cleanup(func() { keychainRead = oldRead })
	p := New(WithHome(home), WithUseKeychain(true), WithKeychainAccount("tester"))
	if !p.Available() {
		t.Error("Available should be true when the Keychain may hold the login")
	}
}

func TestFetchWritebackKeepsRotationDuringRedeem(t *testing.T) {
	home := t.TempDir()
	writeCreds(t, home, "old-access", "old-refresh", pastMs(time.Hour), "pro")
	s := newStub(t)
	s.usage = func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer cli-access" {
			t.Errorf("usage auth = %q, want the CLI-rotated token", r.Header.Get("Authorization"))
		}
		w.Write([]byte(`{"five_hour": {"utilization": 10.0}}`))
	}
	s.refresh = func(w http.ResponseWriter, r *http.Request) {
		// Claude Code rotates the file while the redeem is in flight.
		writeCreds(t, home, "cli-access", "cli-refresh", futureMs(time.Hour), "pro")
		w.Write([]byte(`{"access_token": "new-access", "refresh_token": "new-refresh", "expires_in": 3600}`))
	}
	if _, err := testProvider(home, s, nil).Fetch(context.Background()); err != nil {
		t.Fatal(err)
	}
	raw, _ := os.ReadFile(filepath.Join(home, ".claude", ".credentials.json"))
	var decoded map[string]any
	if err := json.Unmarshal(raw, &decoded); err != nil {
		t.Fatal(err)
	}
	oauth := decoded["claudeAiOauth"].(map[string]any)
	if oauth["accessToken"] != "cli-access" || oauth["refreshToken"] != "cli-refresh" {
		t.Errorf("rotation overwritten on writeback: %v", oauth)
	}
}

func TestFetchInvalidGrantPicksUpRotatedCredentials(t *testing.T) {
	home := t.TempDir()
	writeCreds(t, home, "stale-access", "stale-refresh", pastMs(time.Hour), "pro")
	s := newStub(t)
	s.usage = func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer cli-access" {
			t.Errorf("usage auth = %q, want the CLI-rotated token", r.Header.Get("Authorization"))
		}
		w.Write([]byte(`{"five_hour": {"utilization": 20.0}}`))
	}
	s.refresh = func(w http.ResponseWriter, r *http.Request) {
		if r.FormValue("refresh_token") == "stale-refresh" {
			writeCreds(t, home, "cli-access", "cli-refresh", futureMs(time.Hour), "pro")
			w.WriteHeader(http.StatusBadRequest)
			w.Write([]byte(`{"error": "invalid_grant"}`))
			return
		}
		t.Errorf("unexpected refresh for %q", r.FormValue("refresh_token"))
	}
	now := time.Now()
	p := New(WithHome(home),
		WithUsageURL(s.server.URL+"/api/oauth/usage"),
		WithRefreshURL(s.server.URL+"/v1/oauth/token"),
		WithUseKeychain(false), WithSnapshotTTL(0),
		WithNow(func() time.Time { return now }))
	snap, err := p.Fetch(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if q := quotaByKey(snap.Quotas, "session"); q == nil || *q.PercentRemaining != 80 {
		t.Errorf("session = %+v, want 80", q)
	}
	// The dead token stays terminal: restoring it must not redeem again.
	writeCreds(t, home, "stale-access", "stale-refresh", pastMs(time.Hour), "pro")
	if _, err := p.Fetch(context.Background()); err == nil ||
		!strings.Contains(err.Error(), "log in to Claude Code again") {
		t.Errorf("err = %v, want a log-in-again error", err)
	}
	if got := s.refreshHit.Load(); got != 1 {
		t.Errorf("refresh hits = %d, want 1 (dead token stays terminal)", got)
	}
}
