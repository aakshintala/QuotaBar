package muse_test

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"quotad/monitor"
	"quotad/providers/muse"
)

var testMoment = time.Unix(2_000_000_000, 0).UTC()

// homeLookup resolves HOME to home and nothing else, so tests never read
// the real home directory.
func homeLookup(home string, extra map[string]string) func(string) string {
	return func(k string) string {
		if k == "HOME" {
			return home
		}
		return extra[k]
	}
}

// homeAuth writes meta to home/.config/muse/auth.json and returns its path.
func homeAuth(t *testing.T, home string, meta map[string]any) string {
	t.Helper()
	// schema_version sits beside providers in the real CLI file.
	data, err := json.Marshal(map[string]any{"schema_version": 1, "providers": map[string]any{"meta": meta}})
	if err != nil {
		t.Fatal(err)
	}
	dir := filepath.Join(home, ".config", "muse")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(dir, "auth.json")
	if err := os.WriteFile(path, data, 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}

func oauthMeta(extra map[string]any) map[string]any {
	meta := map[string]any{
		"mechanism":    "oauth",
		"access_token": "dca:test-oauth-token",
		"api_key":      "testdata-standing-key-0000",
	}
	for k, v := range extra {
		meta[k] = v
	}
	return meta
}

// fixtureServer serves body with status and counts hits.
func fixtureServer(t *testing.T, body string, status int, hits *atomic.Int64, check func(r *http.Request)) *httptest.Server {
	t.Helper()
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits.Add(1)
		if check != nil {
			check(r)
		}
		w.WriteHeader(status)
		_, _ = w.Write([]byte(body))
	}))
}

func fixtureBody(t *testing.T) string {
	t.Helper()
	data, err := os.ReadFile(filepath.Join("testdata", "muse-response.json"))
	if err != nil {
		t.Fatal(err)
	}
	return string(data)
}

// deadServerURL is a URL nothing listens on, so requests fail.
func deadServerURL(t *testing.T) string {
	t.Helper()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {}))
	url := srv.URL
	srv.Close()
	return url
}

func newProvider(t *testing.T, home, baseURL string, now *time.Time, keychain func(string, string) ([]byte, error)) *muse.Provider {
	t.Helper()
	opts := []muse.Option{
		muse.WithBaseURL(baseURL),
		muse.WithNow(func() time.Time { return *now }),
		muse.WithLookupEnv(homeLookup(home, nil)),
	}
	if keychain != nil {
		opts = append(opts, muse.WithKeychainRead(keychain))
	}
	return muse.New(opts...)
}

func fetchOK(t *testing.T, home string, meta map[string]any, now time.Time) (*muse.Provider, *atomic.Int64) {
	t.Helper()
	homeAuth(t, home, meta)
	nowCopy := now
	var hits atomic.Int64
	srv := fixtureServer(t, fixtureBody(t), http.StatusOK, &hits, nil)
	t.Cleanup(srv.Close)
	return newProvider(t, home, srv.URL, &nowCopy, nil), &hits
}

func cacheFiles(t *testing.T, home string) []os.DirEntry {
	t.Helper()
	entries, err := os.ReadDir(filepath.Join(home, ".quotabar", "muse"))
	if err != nil {
		t.Fatal(err)
	}
	return entries
}

func TestParsesFixtureAndDiscardsAccountData(t *testing.T) {
	home := t.TempDir()
	p, _ := fetchOK(t, home, oauthMeta(nil), testMoment)
	snap, err := p.Fetch(t.Context())
	if err != nil {
		t.Fatalf("Fetch: %v", err)
	}
	if snap.ProviderID != "muse" {
		t.Errorf("ProviderID = %q", snap.ProviderID)
	}
	if snap.Tier == nil || snap.Tier.Badge != "Muse Pro" {
		t.Errorf("Tier = %+v", snap.Tier)
	}
	if !snap.CapturedAt.Equal(testMoment) {
		t.Errorf("CapturedAt = %v", snap.CapturedAt)
	}
	if len(snap.Quotas) != 2 {
		t.Fatalf("quotas = %d, want 2", len(snap.Quotas))
	}
	if got := *snap.Quotas[0].PercentRemaining; got != 75 {
		t.Errorf("session remaining = %v, want 75", got)
	}
	if snap.Quotas[0].Type.Kind != monitor.KindSession {
		t.Errorf("quota[0] kind = %v", snap.Quotas[0].Type.Kind)
	}
	if got := *snap.Quotas[1].PercentRemaining; got != 60 {
		t.Errorf("weekly remaining = %v, want 60", got)
	}
	if snap.Quotas[1].Type.Kind != monitor.KindWeekly {
		t.Errorf("quota[1] kind = %v", snap.Quotas[1].Type.Kind)
	}
	if got := snap.Quotas[0].ResetsAt.Unix(); got != 2000000300 {
		t.Errorf("session reset = %v", got)
	}
}

func TestRequestUsesFixedOAuthEndpoint(t *testing.T) {
	home := t.TempDir()
	homeAuth(t, home, oauthMeta(nil))
	now := testMoment
	var hits atomic.Int64
	srv := fixtureServer(t, fixtureBody(t), http.StatusOK, &hits, func(r *http.Request) {
		if r.Method != http.MethodPost {
			t.Errorf("method = %s, want POST", r.Method)
		}
		if r.URL.Path != "/muse-code/key" {
			t.Errorf("path = %s", r.URL.Path)
		}
		if got := r.Header.Get("Authorization"); got != "Bearer dca:test-oauth-token" {
			t.Errorf("Authorization = %q", got)
		}
		if got := r.Header.Get("x-api-version"); got != "1.0.0" {
			t.Errorf("x-api-version = %q", got)
		}
		body, _ := io.ReadAll(r.Body)
		if string(body) != `{"onboard":false}` {
			t.Errorf("body = %q", body)
		}
	})
	t.Cleanup(srv.Close)
	p := newProvider(t, home, srv.URL, &now, nil)
	if _, err := p.Fetch(t.Context()); err != nil {
		t.Fatalf("Fetch: %v", err)
	}
	if hits.Load() != 1 {
		t.Errorf("hits = %d, want 1", hits.Load())
	}
}

func TestCredentialFileIsReadOnly(t *testing.T) {
	home := t.TempDir()
	auth := homeAuth(t, home, oauthMeta(nil))
	p, _ := fetchOK(t, home, oauthMeta(nil), testMoment)
	before, err := os.ReadFile(auth)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := p.Fetch(t.Context()); err != nil {
		t.Fatalf("Fetch: %v", err)
	}
	after, err := os.ReadFile(auth)
	if err != nil {
		t.Fatal(err)
	}
	if string(before) != string(after) {
		t.Error("auth file was modified by Fetch")
	}
}

func TestCacheFilesHoldNoSecrets(t *testing.T) {
	home := t.TempDir()
	p, _ := fetchOK(t, home, oauthMeta(nil), testMoment)
	if _, err := p.Fetch(t.Context()); err != nil {
		t.Fatalf("Fetch: %v", err)
	}
	entries := cacheFiles(t, home)
	if len(entries) == 0 {
		t.Fatal("no cache files written")
	}
	for _, e := range entries {
		data, err := os.ReadFile(filepath.Join(home, ".quotabar", "muse", e.Name()))
		if err != nil {
			t.Fatal(err)
		}
		for _, secret := range []string{"dca:test-oauth-token", "testdata-standing-key", "example.invalid"} {
			if strings.Contains(string(data), secret) {
				t.Errorf("%s contains %q", e.Name(), secret)
			}
		}
	}
}

func TestCooldownServesLastObservationWithoutNewRequest(t *testing.T) {
	home := t.TempDir()
	p, hits := fetchOK(t, home, oauthMeta(nil), testMoment)
	first, err := p.Fetch(t.Context())
	if err != nil {
		t.Fatalf("first Fetch: %v", err)
	}
	// Inside QuotaBar's own five-minute cooldown the server has not
	// throttled anything: serve the last reading, no request, no error.
	snap, err := p.Fetch(t.Context())
	if err != nil {
		t.Fatalf("cooldown Fetch: %v, want nil (own cooldown is not a rate limit)", err)
	}
	if !snap.CapturedAt.Equal(first.CapturedAt) || len(snap.Quotas) != len(first.Quotas) {
		t.Errorf("cooldown snapshot = %+v, want the first reading", snap)
	}
	if hits.Load() != 1 {
		t.Errorf("hits = %d, want 1", hits.Load())
	}
}

func TestFailedFetchConsumesCooldownAndKeepsItsError(t *testing.T) {
	home := t.TempDir()
	homeAuth(t, home, oauthMeta(nil))
	now := testMoment
	var hits atomic.Int64
	srv := fixtureServer(t, "boom", http.StatusInternalServerError, &hits, nil)
	t.Cleanup(srv.Close)
	p := newProvider(t, home, srv.URL, &now, nil)
	_, firstErr := p.Fetch(t.Context())
	if firstErr == nil {
		t.Fatal("expected fetch error")
	}
	// The failure reserves the cooldown: no second request, and the
	// refresh keeps reporting the real failure, not a rate limit.
	_, err := p.Fetch(t.Context())
	var rl *monitor.RateLimitedError
	if err == nil || errors.As(err, &rl) || err.Error() != firstErr.Error() {
		t.Fatalf("second error = %v, want the first failure %v", err, firstErr)
	}
	if hits.Load() != 1 {
		t.Errorf("hits = %d, want 1", hits.Load())
	}
}

func TestFailureAfterSuccessKeepsErrorThroughCooldown(t *testing.T) {
	home := t.TempDir()
	homeAuth(t, home, oauthMeta(nil))
	now := testMoment
	var hits atomic.Int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if hits.Add(1) == 1 {
			_, _ = w.Write([]byte(fixtureBody(t)))
			return
		}
		w.WriteHeader(http.StatusInternalServerError)
	}))
	t.Cleanup(srv.Close)
	p := newProvider(t, home, srv.URL, &now, nil)
	if _, err := p.Fetch(t.Context()); err != nil {
		t.Fatalf("first Fetch: %v", err)
	}
	now = testMoment.Add(301 * time.Second)
	if _, err := p.Fetch(t.Context()); err == nil {
		t.Fatal("expected failure")
	}
	// Inside the cooldown after that failure the error must stay visible
	// (the monitor keeps the old reading beside it), not clear.
	now = now.Add(30 * time.Second)
	if _, err := p.Fetch(t.Context()); err == nil {
		t.Fatal("cooldown after a failure cleared the error")
	}
	if hits.Load() != 2 {
		t.Errorf("hits = %d, want 2", hits.Load())
	}
}

func TestAfterRestartServesPersistedObservation(t *testing.T) {
	home := t.TempDir()
	homeAuth(t, home, oauthMeta(nil))
	first := testMoment
	var hits atomic.Int64
	srv := fixtureServer(t, fixtureBody(t), http.StatusOK, &hits, nil)
	t.Cleanup(srv.Close)
	firstSnap, err := newProvider(t, home, srv.URL, &first, nil).Fetch(t.Context())
	if err != nil {
		t.Fatalf("first Fetch: %v", err)
	}

	// A fresh provider (no in-memory snapshot) whose fetch fails serves the
	// unexpired persisted observation with a nil error.
	second := testMoment.Add(350 * time.Second)
	p2 := muse.New(
		muse.WithBaseURL(deadServerURL(t)),
		muse.WithNow(func() time.Time { return second }),
		muse.WithLookupEnv(homeLookup(home, nil)),
	)
	snap, err := p2.Fetch(t.Context())
	if err != nil {
		t.Fatalf("restart Fetch: %v", err)
	}
	// The session window (reset 2000000300) expired by +350s; weekly remains.
	if len(snap.Quotas) != 1 || snap.Quotas[0].Type.Kind != monitor.KindWeekly {
		t.Fatalf("quotas = %+v, want weekly only", snap.Quotas)
	}
	if !snap.CapturedAt.Equal(firstSnap.CapturedAt) {
		t.Errorf("CapturedAt = %v, want %v", snap.CapturedAt, firstSnap.CapturedAt)
	}
	want := quotaOfKind(firstSnap, monitor.KindWeekly)
	if got := *snap.Quotas[0].PercentRemaining; want == nil || got != *want.PercentRemaining {
		t.Errorf("restored weekly remaining = %v, want %v", got, want)
	}
}

func quotaOfKind(s monitor.Snapshot, k monitor.QuotaKind) *monitor.Quota {
	for i := range s.Quotas {
		if s.Quotas[i].Type.Kind == k {
			return &s.Quotas[i]
		}
	}
	return nil
}

func TestRestartInsideCooldownServesPersistedObservation(t *testing.T) {
	home := t.TempDir()
	p, hits := fetchOK(t, home, oauthMeta(nil), testMoment)
	if _, err := p.Fetch(t.Context()); err != nil {
		t.Fatalf("first Fetch: %v", err)
	}

	// A restart inside the cooldown has no in-memory snapshot, so it serves
	// the persisted observation instead of a bare rate-limit error, and does
	// not hit the API.
	second := testMoment.Add(30 * time.Second)
	p2 := muse.New(
		muse.WithBaseURL(deadServerURL(t)),
		muse.WithNow(func() time.Time { return second }),
		muse.WithLookupEnv(homeLookup(home, nil)),
	)
	snap, err := p2.Fetch(t.Context())
	if err != nil {
		t.Fatalf("restart Fetch: %v", err)
	}
	if len(snap.Quotas) == 0 {
		t.Fatal("restart Fetch returned no quotas")
	}
	if hits.Load() != 1 {
		t.Errorf("hits = %d, want 1", hits.Load())
	}
}

func TestFailureWithMemoryReturnsError(t *testing.T) {
	home := t.TempDir()
	homeAuth(t, home, oauthMeta(nil))
	now := testMoment
	var hits atomic.Int64
	// First hit succeeds; later hits fail. The same provider holds the
	// first snapshot in memory, so the failed refresh must surface the
	// error and let the monitor keep its previous snapshot.
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if hits.Add(1) == 1 {
			_, _ = w.Write([]byte(fixtureBody(t)))
			return
		}
		w.WriteHeader(http.StatusInternalServerError)
	}))
	t.Cleanup(srv.Close)
	p := newProvider(t, home, srv.URL, &now, nil)
	if _, err := p.Fetch(t.Context()); err != nil {
		t.Fatalf("first Fetch: %v", err)
	}
	now = testMoment.Add(301 * time.Second)
	if _, err := p.Fetch(t.Context()); err == nil {
		t.Fatal("expected fetch error when memory exists")
	}
}

func TestExpiredPersistedObservationReturnsError(t *testing.T) {
	home := t.TempDir()
	homeAuth(t, home, oauthMeta(nil))
	first := testMoment
	var hits atomic.Int64
	srv := fixtureServer(t, fixtureBody(t), http.StatusOK, &hits, nil)
	t.Cleanup(srv.Close)
	if _, err := newProvider(t, home, srv.URL, &first, nil).Fetch(t.Context()); err != nil {
		t.Fatalf("first Fetch: %v", err)
	}
	// Both windows reset long before now: nothing unexpired remains.
	longAfter := time.Unix(2_001_000_000, 0).UTC()
	p2 := muse.New(
		muse.WithBaseURL(deadServerURL(t)),
		muse.WithNow(func() time.Time { return longAfter }),
		muse.WithLookupEnv(homeLookup(home, nil)),
	)
	if _, err := p2.Fetch(t.Context()); err == nil {
		t.Fatal("expected error when persisted observation expired")
	}
}

func TestStatus429HonoursRetryAfter(t *testing.T) {
	home := t.TempDir()
	homeAuth(t, home, oauthMeta(nil))
	now := testMoment
	var hits atomic.Int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits.Add(1)
		w.Header().Set("Retry-After", "120")
		w.WriteHeader(http.StatusTooManyRequests)
	}))
	t.Cleanup(srv.Close)
	p := newProvider(t, home, srv.URL, &now, nil)
	_, err := p.Fetch(t.Context())
	if err == nil {
		t.Fatal("expected rate-limited error")
	}
	var rl *monitor.RateLimitedError
	if !errors.As(err, &rl) {
		t.Fatalf("error = %T (%v)", err, err)
	}
	if !rl.RetryAt.Equal(now.Add(120 * time.Second)) {
		t.Errorf("RetryAt = %v, want now+120s", rl.RetryAt)
	}
}

func TestStatus401TellsUserToLogInAgain(t *testing.T) {
	for _, status := range []int{http.StatusUnauthorized, http.StatusForbidden} {
		home := t.TempDir()
		homeAuth(t, home, oauthMeta(nil))
		now := testMoment
		var hits atomic.Int64
		srv := fixtureServer(t, "denied", status, &hits, nil)
		t.Cleanup(srv.Close)
		p := newProvider(t, home, srv.URL, &now, nil)
		_, err := p.Fetch(t.Context())
		if err == nil {
			t.Fatalf("status %d: expected error", status)
		}
		if !strings.Contains(err.Error(), "muse login") {
			t.Errorf("status %d: error = %q, want muse login hint", status, err)
		}
	}
}

func TestRedirectIsRefused(t *testing.T) {
	home := t.TempDir()
	homeAuth(t, home, oauthMeta(nil))
	now := testMoment
	var targetHits atomic.Int64
	target := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		targetHits.Add(1)
	}))
	t.Cleanup(target.Close)
	var hits atomic.Int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits.Add(1)
		http.Redirect(w, r, target.URL+"/secret", http.StatusFound)
	}))
	t.Cleanup(srv.Close)
	p := newProvider(t, home, srv.URL, &now, nil)
	if _, err := p.Fetch(t.Context()); err == nil {
		t.Fatal("expected redirect error")
	}
	if targetHits.Load() != 0 {
		t.Error("redirect target was requested")
	}
}

func TestOversizedBodyIsRejected(t *testing.T) {
	home := t.TempDir()
	homeAuth(t, home, oauthMeta(nil))
	now := testMoment
	var hits atomic.Int64
	srv := fixtureServer(t, strings.Repeat("a", 300*1024), http.StatusOK, &hits, nil)
	t.Cleanup(srv.Close)
	p := newProvider(t, home, srv.URL, &now, nil)
	_, err := p.Fetch(t.Context())
	if err == nil || !strings.Contains(err.Error(), "size limit") {
		t.Fatalf("error = %v, want size-limit error", err)
	}
}

func TestInactiveSubscriptionIsAnError(t *testing.T) {
	home := t.TempDir()
	homeAuth(t, home, oauthMeta(nil))
	now := testMoment
	var hits atomic.Int64
	srv := fixtureServer(t, `{"is_subs_active":false}`, http.StatusOK, &hits, nil)
	t.Cleanup(srv.Close)
	p := newProvider(t, home, srv.URL, &now, nil)
	_, err := p.Fetch(t.Context())
	if err == nil || !strings.Contains(err.Error(), "not active") {
		t.Fatalf("error = %v, want inactive-subscription error", err)
	}
}

func TestMissingWindowsAreUnknownRatherThanFull(t *testing.T) {
	home := t.TempDir()
	homeAuth(t, home, oauthMeta(nil))
	now := testMoment
	var hits atomic.Int64
	srv := fixtureServer(t, `{"is_subs_active":true}`, http.StatusOK, &hits, nil)
	t.Cleanup(srv.Close)
	p := newProvider(t, home, srv.URL, &now, nil)
	snap, err := p.Fetch(t.Context())
	if err != nil {
		t.Fatalf("Fetch: %v", err)
	}
	if len(snap.Quotas) != 0 {
		t.Errorf("quotas = %d, want 0", len(snap.Quotas))
	}
}

func TestExpiredWindowIsSkipped(t *testing.T) {
	home := t.TempDir()
	homeAuth(t, home, oauthMeta(nil))
	now := testMoment
	body := `{"is_subs_active":true,"subs_usage":{"window":{"used_percent":0,"resets_at":1999999999},"weekly":{"used_percent":20,"resets_at":2000000001}}}`
	var hits atomic.Int64
	srv := fixtureServer(t, body, http.StatusOK, &hits, nil)
	t.Cleanup(srv.Close)
	p := newProvider(t, home, srv.URL, &now, nil)
	snap, err := p.Fetch(t.Context())
	if err != nil {
		t.Fatalf("Fetch: %v", err)
	}
	if len(snap.Quotas) != 1 || snap.Quotas[0].Type.Kind != monitor.KindWeekly {
		t.Fatalf("quotas = %+v, want weekly only", snap.Quotas)
	}
}

func TestUsedPercentIsClamped(t *testing.T) {
	for _, tc := range []struct{ used, want float64 }{
		{150, 0},
		{-20, 100},
	} {
		home := t.TempDir()
		homeAuth(t, home, oauthMeta(nil))
		now := testMoment
		var hits atomic.Int64
		body := fmt.Sprintf(`{"is_subs_active":true,"subs_usage":{"window":{"used_percent":%v,"resets_at":2000000300}}}`, tc.used)
		srv := fixtureServer(t, body, http.StatusOK, &hits, nil)
		t.Cleanup(srv.Close)
		p := newProvider(t, home, srv.URL, &now, nil)
		snap, err := p.Fetch(t.Context())
		if err != nil {
			t.Fatalf("used=%v: %v", tc.used, err)
		}
		if len(snap.Quotas) != 1 || *snap.Quotas[0].PercentRemaining != tc.want {
			t.Errorf("used=%v: quotas = %+v, want remaining %v", tc.used, snap.Quotas, tc.want)
		}
	}
}

func TestNonOAuthMechanismIsUnavailable(t *testing.T) {
	home := t.TempDir()
	homeAuth(t, home, oauthMeta(map[string]any{"mechanism": "api_key"}))
	now := testMoment
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		t.Error("no request should be made without OAuth credentials")
	}))
	t.Cleanup(srv.Close)
	p := newProvider(t, home, srv.URL, &now, nil)
	_, err := p.Fetch(t.Context())
	if err == nil || !strings.Contains(err.Error(), "missing or malformed") {
		t.Fatalf("error = %v, want missing/malformed credentials", err)
	}
}

func TestMissingStandingKeyIsActionable(t *testing.T) {
	home := t.TempDir()
	meta := oauthMeta(nil)
	delete(meta, "api_key")
	homeAuth(t, home, meta)
	now := testMoment
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		t.Error("no request should be made without the standing key")
	}))
	t.Cleanup(srv.Close)
	p := newProvider(t, home, srv.URL, &now, nil)
	_, err := p.Fetch(t.Context())
	if err == nil || !strings.Contains(err.Error(), "standing API key") {
		t.Fatalf("error = %v, want standing-key error", err)
	}
}

func TestMalformedTokenIsActionable(t *testing.T) {
	home := t.TempDir()
	homeAuth(t, home, oauthMeta(map[string]any{"access_token": "bogus"}))
	now := testMoment
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		t.Error("no request should be made with a malformed token")
	}))
	t.Cleanup(srv.Close)
	p := newProvider(t, home, srv.URL, &now, nil)
	_, err := p.Fetch(t.Context())
	if err == nil || !strings.Contains(err.Error(), "access token") {
		t.Fatalf("error = %v, want access-token error", err)
	}
}

func TestKeychainStorageReadsExactItem(t *testing.T) {
	home := t.TempDir()
	homeAuth(t, home, map[string]any{
		"mechanism": "oauth",
		"storage":   "keychain",
	})
	var gotService, gotAccount string
	keychain := func(service, account string) ([]byte, error) {
		gotService, gotAccount = service, account
		return []byte(`{"access_token":"dca:keychain-oauth","api_key":"testdata-standing-key-0000"}`), nil
	}
	now := testMoment
	var hits atomic.Int64
	srv := fixtureServer(t, fixtureBody(t), http.StatusOK, &hits, func(r *http.Request) {
		if got := r.Header.Get("Authorization"); got != "Bearer dca:keychain-oauth" {
			t.Errorf("Authorization = %q", got)
		}
	})
	t.Cleanup(srv.Close)
	p := newProvider(t, home, srv.URL, &now, keychain)
	if _, err := p.Fetch(t.Context()); err != nil {
		t.Fatalf("Fetch: %v", err)
	}
	if gotService != "ai.meta.dev.credentials" || gotAccount != "meta" {
		t.Errorf("keychain item = %q/%q", gotService, gotAccount)
	}
}

func TestKeychainFailuresAreActionable(t *testing.T) {
	for _, tc := range []struct {
		name    string
		stub    func(string, string) ([]byte, error)
		want    []string
		notWant string
	}{
		{"not found", func(string, string) ([]byte, error) {
			return nil, muse.ErrKeychainNotFound
		}, []string{"Keychain", "missing"}, ""},
		{"denied", func(string, string) ([]byte, error) {
			return nil, muse.ErrKeychainDenied
		}, []string{"Keychain", "access"}, ""},
		{"generic", func(string, string) ([]byte, error) {
			return nil, errors.New("security failed")
		}, []string{"Keychain"}, "status"},
		{"empty", func(string, string) ([]byte, error) {
			return nil, nil
		}, []string{"malformed"}, ""},
		{"malformed", func(string, string) ([]byte, error) {
			return []byte(`{"access_token":42}`), nil
		}, []string{"malformed"}, ""},
	} {
		t.Run(tc.name, func(t *testing.T) {
			home := t.TempDir()
			homeAuth(t, home, map[string]any{"mechanism": "oauth", "storage": "keychain"})
			now := testMoment
			srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				t.Error("no request should be made when the Keychain read fails")
			}))
			t.Cleanup(srv.Close)
			p := newProvider(t, home, srv.URL, &now, tc.stub)
			_, err := p.Fetch(t.Context())
			if err == nil {
				t.Fatal("expected error")
			}
			for _, want := range tc.want {
				if !strings.Contains(err.Error(), want) {
					t.Errorf("error = %q, want %q", err, want)
				}
			}
			if tc.notWant != "" && strings.Contains(err.Error(), tc.notWant) {
				t.Errorf("error = %q, must not contain %q", err, tc.notWant)
			}
		})
	}
}

func TestMissingFileOnDarwinFallsBackToKeychain(t *testing.T) {
	home := t.TempDir() // no auth file
	now := testMoment
	var hits atomic.Int64
	srv := fixtureServer(t, fixtureBody(t), http.StatusOK, &hits, nil)
	t.Cleanup(srv.Close)
	p := muse.New(
		muse.WithBaseURL(srv.URL),
		muse.WithNow(func() time.Time { return now }),
		muse.WithLookupEnv(homeLookup(home, nil)),
		muse.WithGOOS("darwin"),
		muse.WithKeychainRead(func(string, string) ([]byte, error) {
			return []byte(`{"access_token":"dca:keychain-oauth","api_key":"testdata-standing-key-0000"}`), nil
		}),
	)
	if _, err := p.Fetch(t.Context()); err != nil {
		t.Fatalf("Fetch: %v", err)
	}
	if hits.Load() != 1 {
		t.Errorf("hits = %d, want 1", hits.Load())
	}
}

func TestMissingFileOnLinuxIsUnavailable(t *testing.T) {
	home := t.TempDir() // no auth file
	now := testMoment
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		t.Error("no request should be made without credentials")
	}))
	t.Cleanup(srv.Close)
	p := muse.New(
		muse.WithBaseURL(srv.URL),
		muse.WithNow(func() time.Time { return now }),
		muse.WithLookupEnv(homeLookup(home, nil)),
		muse.WithGOOS("linux"),
	)
	if p.Available() {
		t.Error("Available = true with no credential source")
	}
	if _, err := p.Fetch(t.Context()); err == nil ||
		!strings.Contains(err.Error(), "missing or malformed") {
		t.Fatalf("error = %v, want missing/malformed credentials", err)
	}
}

func TestAvailableChecksCredentialSource(t *testing.T) {
	home := t.TempDir()
	homeAuth(t, home, oauthMeta(nil))
	p := muse.New(
		muse.WithLookupEnv(homeLookup(home, nil)),
		muse.WithGOOS("linux"),
	)
	if !p.Available() {
		t.Error("Available = false with auth file present")
	}
	empty := t.TempDir()
	missing := muse.New(
		muse.WithLookupEnv(homeLookup(empty, nil)),
		muse.WithGOOS("darwin"),
	)
	if !missing.Available() {
		t.Error("Available = false on darwin (Keychain may hold the login)")
	}
}

func TestAuthPathResolution(t *testing.T) {
	t.Run("absolute XDG", func(t *testing.T) {
		xdg := t.TempDir()
		home := t.TempDir()
		authDir := filepath.Join(xdg, "muse")
		if err := os.MkdirAll(authDir, 0o755); err != nil {
			t.Fatal(err)
		}
		homeAuthXDG(t, authDir)
		p := muse.New(
			muse.WithGOOS("linux"),
			muse.WithLookupEnv(homeLookup(home, map[string]string{"XDG_CONFIG_HOME": xdg})),
		)
		if !p.Available() {
			t.Error("Available = false for absolute XDG_CONFIG_HOME")
		}
	})
	t.Run("relative XDG falls back to home", func(t *testing.T) {
		home := t.TempDir()
		homeAuth(t, home, oauthMeta(nil))
		p := muse.New(
			muse.WithGOOS("linux"),
			muse.WithLookupEnv(homeLookup(home, map[string]string{"XDG_CONFIG_HOME": "relative/path"})),
		)
		if !p.Available() {
			t.Error("Available = false for ~/.config fallback")
		}
	})
}

// homeAuthXDG writes an OAuth record to an explicit auth dir (XDG layout).
func homeAuthXDG(t *testing.T, dir string) {
	t.Helper()
	data, err := json.Marshal(map[string]any{"providers": map[string]any{"meta": oauthMeta(nil)}})
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "auth.json"), data, 0o600); err != nil {
		t.Fatal(err)
	}
}

func TestID(t *testing.T) {
	if id := muse.New().ID(); id != "muse" {
		t.Errorf("ID = %q", id)
	}
}

func TestRetryAfterPersistsPastCooldown(t *testing.T) {
	home := t.TempDir()
	homeAuth(t, home, oauthMeta(nil))
	now := testMoment
	var hits atomic.Int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits.Add(1)
		w.Header().Set("Retry-After", "3600")
		w.WriteHeader(http.StatusTooManyRequests)
	}))
	t.Cleanup(srv.Close)
	p := newProvider(t, home, srv.URL, &now, nil)
	if _, err := p.Fetch(t.Context()); err == nil {
		t.Fatal("expected rate-limited error")
	}
	// Past the five-minute cooldown but before the server's retry time:
	// no new request may go out.
	now = testMoment.Add(301 * time.Second)
	_, err := p.Fetch(t.Context())
	var rl *monitor.RateLimitedError
	if !errors.As(err, &rl) {
		t.Fatalf("error = %v, want RateLimitedError", err)
	}
	if !rl.RetryAt.Equal(testMoment.Add(3600 * time.Second)) {
		t.Errorf("RetryAt = %v, want testMoment+3600s", rl.RetryAt)
	}
	if hits.Load() != 1 {
		t.Errorf("hits = %d, want 1", hits.Load())
	}
}

func TestEmptyResponsePreservesPreviousObservation(t *testing.T) {
	home := t.TempDir()
	homeAuth(t, home, oauthMeta(nil))
	now := testMoment
	var hits atomic.Int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if hits.Add(1) == 1 {
			_, _ = w.Write([]byte(fixtureBody(t)))
			return
		}
		_, _ = w.Write([]byte(`{"is_subs_active":true}`))
	}))
	t.Cleanup(srv.Close)
	p := newProvider(t, home, srv.URL, &now, nil)
	if _, err := p.Fetch(t.Context()); err != nil {
		t.Fatalf("first Fetch: %v", err)
	}
	now = testMoment.Add(301 * time.Second)
	if _, err := p.Fetch(t.Context()); err == nil {
		t.Fatal("expected error for a quota-less response with a previous observation")
	} else if !strings.Contains(err.Error(), "last known") {
		t.Fatalf("error = %q, want last-known-reading hint", err)
	}
	// The persisted observation must be untouched: a fresh provider whose
	// fetch fails past both cooldowns still serves the previous windows.
	restart := testMoment.Add(650 * time.Second)
	p2 := muse.New(
		muse.WithBaseURL(deadServerURL(t)),
		muse.WithNow(func() time.Time { return restart }),
		muse.WithLookupEnv(homeLookup(home, nil)),
	)
	snap, err := p2.Fetch(t.Context())
	if err != nil {
		t.Fatalf("restart Fetch: %v", err)
	}
	if len(snap.Quotas) != 1 || snap.Quotas[0].Type.Kind != monitor.KindWeekly {
		t.Fatalf("quotas = %+v, want persisted weekly only", snap.Quotas)
	}
}

func TestAuthPathOverrideIsHonoured(t *testing.T) {
	home := t.TempDir() // no auth file here
	xdgDir := filepath.Join(t.TempDir(), "muse")
	if err := os.MkdirAll(xdgDir, 0o755); err != nil {
		t.Fatal(err)
	}
	xdgData, err := json.Marshal(map[string]any{
		"providers": map[string]any{"meta": oauthMeta(map[string]any{"access_token": "dca:xdg-login-token"})},
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(xdgDir, "auth.json"), xdgData, 0o600); err != nil {
		t.Fatal(err)
	}
	override := filepath.Join(t.TempDir(), "custom-auth.json")
	overrideData, err := json.Marshal(map[string]any{"providers": map[string]any{"meta": oauthMeta(nil)}})
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(override, overrideData, 0o600); err != nil {
		t.Fatal(err)
	}
	now := testMoment
	var hits atomic.Int64
	var gotAuth string
	srv := fixtureServer(t, fixtureBody(t), http.StatusOK, &hits, func(r *http.Request) {
		gotAuth = r.Header.Get("Authorization")
	})
	t.Cleanup(srv.Close)
	p := muse.New(
		muse.WithBaseURL(srv.URL),
		muse.WithNow(func() time.Time { return now }),
		muse.WithLookupEnv(homeLookup(home, map[string]string{
			"XDG_CONFIG_HOME": filepath.Dir(xdgDir),
			"MUSE_AUTH_PATH":  override,
		})),
		muse.WithGOOS("linux"),
	)
	if !p.Available() {
		t.Error("Available = false with MUSE_AUTH_PATH set")
	}
	if _, err := p.Fetch(t.Context()); err != nil {
		t.Fatalf("Fetch: %v", err)
	}
	want := "Bearer " + oauthMeta(nil)["access_token"].(string)
	if gotAuth != want {
		t.Errorf("Authorization = %q, want override login %q", gotAuth, want)
	}
}

func TestFractionalResetsArePreserved(t *testing.T) {
	home := t.TempDir()
	homeAuth(t, home, oauthMeta(nil))
	now := testMoment
	body := `{"is_subs_active":true,"subs_usage":{"window":{"used_percent":25,"resets_at":2000000300.5},"weekly":{"used_percent":40,"resets_at":2000600000.25}}}`
	var hits atomic.Int64
	srv := fixtureServer(t, body, http.StatusOK, &hits, nil)
	t.Cleanup(srv.Close)
	p := newProvider(t, home, srv.URL, &now, nil)
	snap, err := p.Fetch(t.Context())
	if err != nil {
		t.Fatalf("Fetch: %v", err)
	}
	if len(snap.Quotas) != 2 {
		t.Fatalf("quotas = %d, want 2", len(snap.Quotas))
	}
	wantSession := time.Unix(2000000300, int64(500*time.Millisecond)).UTC()
	if !snap.Quotas[0].ResetsAt.Equal(wantSession) {
		t.Errorf("session reset = %v, want %v", snap.Quotas[0].ResetsAt, wantSession)
	}
	wantWeekly := time.Unix(2000600000, int64(250*time.Millisecond)).UTC()
	if !snap.Quotas[1].ResetsAt.Equal(wantWeekly) {
		t.Errorf("weekly reset = %v, want %v", snap.Quotas[1].ResetsAt, wantWeekly)
	}
	// A restart past the cooldown serves the persisted weekly window with
	// its fraction intact (the session window has expired by then).
	restart := testMoment.Add(350 * time.Second)
	p2 := muse.New(
		muse.WithBaseURL(deadServerURL(t)),
		muse.WithNow(func() time.Time { return restart }),
		muse.WithLookupEnv(homeLookup(home, nil)),
	)
	served, err := p2.Fetch(t.Context())
	if err != nil {
		t.Fatalf("restart Fetch: %v", err)
	}
	if len(served.Quotas) != 1 || !served.Quotas[0].ResetsAt.Equal(wantWeekly) {
		t.Errorf("persisted weekly reset = %v, want %v", served.Quotas, wantWeekly)
	}
}

func TestDarwinFileErrorDoesNotFallBackToKeychain(t *testing.T) {
	home := t.TempDir()
	// A directory at the auth path: ReadFile fails with a non-NotExist error.
	if err := os.MkdirAll(filepath.Join(home, ".config", "muse", "auth.json"), 0o755); err != nil {
		t.Fatal(err)
	}
	now := testMoment
	called := false
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		t.Error("no request should be made when credentials are unreadable")
	}))
	t.Cleanup(srv.Close)
	p := muse.New(
		muse.WithBaseURL(srv.URL),
		muse.WithNow(func() time.Time { return now }),
		muse.WithLookupEnv(homeLookup(home, nil)),
		muse.WithGOOS("darwin"),
		muse.WithKeychainRead(func(string, string) ([]byte, error) {
			called = true
			return []byte(`{"access_token":"dca:keychain-oauth","api_key":"testdata-standing-key-0000"}`), nil
		}),
	)
	if _, err := p.Fetch(t.Context()); err == nil {
		t.Fatal("expected credential-file error")
	}
	if called {
		t.Error("Keychain must not be consulted for a non-NotExist file error")
	}
}
