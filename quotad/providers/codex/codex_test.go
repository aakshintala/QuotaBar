package codex

import (
	"encoding/base64"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"quotad/monitor"
)

var testNow = time.Date(2026, time.November, 5, 12, 0, 0, 0, time.UTC)

func noEnv(string) string { return "" }

// writeAuth writes an auth.json under dir, honouring CODEX_HOME layout when set.
func writeAuth(t *testing.T, dir, content string) string {
	t.Helper()
	codexDir := filepath.Join(dir, ".codex")
	if err := os.MkdirAll(codexDir, 0o700); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(codexDir, "auth.json")
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}

func authJSON(access, refresh, account, last string) string {
	tokens := map[string]string{"access_token": access}
	if refresh != "" {
		tokens["refresh_token"] = refresh
	}
	if account != "" {
		tokens["account_id"] = account
	}
	doc := map[string]any{"tokens": tokens}
	if last != "" {
		doc["last_refresh"] = last
	}
	data, err := json.Marshal(doc)
	if err != nil {
		panic(err)
	}
	return string(data)
}

// fakeJWT builds an unsigned JWT; exp nil means no exp claim.
func fakeJWT(exp *time.Time) string {
	header := base64.RawURLEncoding.EncodeToString([]byte(`{"alg":"none"}`))
	payload := "{}"
	if exp != nil {
		payload = fmt.Sprintf(`{"sub":"user-fake-1","exp":%d}`, exp.Unix())
	}
	return header + "." + base64.RawURLEncoding.EncodeToString([]byte(payload)) + ".sig"
}

func freshLast() string { return testNow.Format(time.RFC3339) }

func oldLast() string { return testNow.Add(-9 * 24 * time.Hour).Format(time.RFC3339) }

func newTestProvider(dir string, cfg Config) *Provider {
	cfg.HomeDir = dir
	cfg.LookupEnv = noEnv
	cfg.Now = func() time.Time { return testNow }
	return New(cfg)
}

func findQuota(quotas []monitor.Quota, key string) *monitor.Quota {
	for i, q := range quotas {
		if q.Type.Key() == key {
			return &quotas[i]
		}
	}
	return nil
}

func TestParseCapturedResponse(t *testing.T) {
	body, err := os.ReadFile("testdata/usage.json")
	if err != nil {
		t.Fatal(err)
	}
	quotas, tier, err := parseUsage(body, http.Header{}, testNow)
	if err != nil {
		t.Fatal(err)
	}
	session := findQuota(quotas, "session")
	if session == nil || session.PercentRemaining == nil || *session.PercentRemaining != 70 {
		t.Errorf("session quota = %+v, want 70 remaining", session)
	}
	if session == nil || session.ResetsAt == nil || session.ResetsAt.Unix() != 2000003600 {
		t.Errorf("session reset = %+v, want 2000003600", session)
	}
	weekly := findQuota(quotas, "weekly")
	if weekly == nil || weekly.PercentRemaining == nil || *weekly.PercentRemaining != 40 {
		t.Errorf("weekly quota = %+v, want 40 remaining", weekly)
	}
	credits := findQuota(quotas, "time:Credits")
	if credits == nil || credits.BalanceRemaining == nil || *credits.BalanceRemaining != 1234.5 {
		t.Errorf("credits quota = %+v, want 1234.5", credits)
	}
	if credits == nil || !credits.IsBalanceOnly() {
		t.Errorf("credits quota should be balance-only: %+v", credits)
	}
	if tier == nil || tier.Badge != "PLUS" {
		t.Errorf("tier = %+v, want PLUS", tier)
	}
}

func TestWindowsAssignedByLengthNotPosition(t *testing.T) {
	body := `{"rate_limit": {
		"primary_window": {"used_percent": 60, "reset_at": 2000600000, "limit_window_seconds": 604800},
		"secondary_window": {"used_percent": 30, "reset_at": 2000003600, "limit_window_seconds": 18000}
	}}`
	quotas, _, err := parseUsage([]byte(body), http.Header{}, testNow)
	if err != nil {
		t.Fatal(err)
	}
	session := findQuota(quotas, "session")
	if session == nil || *session.PercentRemaining != 70 {
		t.Errorf("session quota = %+v, want 70 (from secondary window)", session)
	}
	weekly := findQuota(quotas, "weekly")
	if weekly == nil || *weekly.PercentRemaining != 40 {
		t.Errorf("weekly quota = %+v, want 40 (from primary window)", weekly)
	}
}

func TestSingleWeeklyLengthWindowTakesWeeklySlot(t *testing.T) {
	body := `{"rate_limit": {
		"primary_window": {"used_percent": 10, "limit_window_seconds": 604800}
	}}`
	quotas, _, err := parseUsage([]byte(body), http.Header{}, testNow)
	if err != nil {
		t.Fatal(err)
	}
	if len(quotas) != 1 || quotas[0].Type.Key() != "weekly" {
		t.Errorf("quotas = %+v, want one weekly quota", quotas)
	}
}

func TestHeaderPercentTakesPrecedence(t *testing.T) {
	body := `{"rate_limit": {
		"primary_window": {"used_percent": 30, "reset_after_seconds": 3600},
		"secondary_window": {"used_percent": 60, "reset_after_seconds": 432000}
	}}`
	header := http.Header{
		"X-Codex-Primary-Used-Percent":   {"25.5"},
		"X-Codex-Secondary-Used-Percent": {"45.0"},
	}
	quotas, _, err := parseUsage([]byte(body), header, testNow)
	if err != nil {
		t.Fatal(err)
	}
	if q := findQuota(quotas, "session"); q == nil || *q.PercentRemaining != 74.5 {
		t.Errorf("session quota = %+v, want 74.5", q)
	}
	if q := findQuota(quotas, "weekly"); q == nil || *q.PercentRemaining != 55 {
		t.Errorf("weekly quota = %+v, want 55", q)
	}
}

func TestHeaderFollowsWindowRole(t *testing.T) {
	body := `{"rate_limit": {
		"primary_window": {"used_percent": 1, "limit_window_seconds": 604800},
		"secondary_window": {"used_percent": 1, "limit_window_seconds": 18000}
	}}`
	header := http.Header{
		"X-Codex-Primary-Used-Percent":   {"60"},
		"X-Codex-Secondary-Used-Percent": {"30"},
	}
	quotas, _, err := parseUsage([]byte(body), header, testNow)
	if err != nil {
		t.Fatal(err)
	}
	if q := findQuota(quotas, "weekly"); q == nil || *q.PercentRemaining != 40 {
		t.Errorf("weekly quota = %+v, want 40 from the primary header", q)
	}
	if q := findQuota(quotas, "session"); q == nil || *q.PercentRemaining != 70 {
		t.Errorf("session quota = %+v, want 70 from the secondary header", q)
	}
}

func TestResetAfterSeconds(t *testing.T) {
	body := `{"rate_limit": {"primary_window": {"used_percent": 10, "reset_after_seconds": 3600}}}`
	quotas, _, err := parseUsage([]byte(body), http.Header{}, testNow)
	if err != nil {
		t.Fatal(err)
	}
	q := findQuota(quotas, "session")
	if q == nil || q.ResetsAt == nil || !q.ResetsAt.Equal(testNow.Add(time.Hour)) {
		t.Errorf("session reset = %+v, want testNow+1h", q)
	}
}

func TestCreditsVariants(t *testing.T) {
	headerBalance := func(v string) http.Header {
		return http.Header{"X-Codex-Credits-Balance": {v}}
	}
	cases := []struct {
		name    string
		body    string
		header  http.Header
		balance float64
		present bool
	}{
		{"header balance", `{"rate_limit":{}}`, headerBalance("750.0"), 750, true},
		{"string body balance", `{"credits": {"has_credits": true, "balance": "1234.5"}}`, nil, 1234.5, true},
		{"numeric body balance", `{"credits": {"balance": 42}}`, nil, 42, true},
		{"no credits section", `{"rate_limit":{}}`, nil, 0, false},
		{"flag false with zero", `{"credits": {"has_credits": false, "balance": "0"}}`, nil, 0, false},
		{"zero without flag", `{"credits": {"balance": 0}}`, nil, 0, false},
		{"flag false with balance", `{"credits": {"has_credits": false, "balance": "12"}}`, nil, 0, false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			quotas, _, err := parseUsage([]byte(tc.body), tc.header, testNow)
			if err != nil {
				t.Fatal(err)
			}
			q := findQuota(quotas, "time:Credits")
			if !tc.present {
				if q != nil {
					t.Errorf("credits quota = %+v, want none", q)
				}
				return
			}
			if q == nil || q.BalanceRemaining == nil || *q.BalanceRemaining != tc.balance {
				t.Errorf("credits quota = %+v, want %v", q, tc.balance)
			}
			if q != nil && (!q.IsBalanceOnly() || q.BalanceUnit == nil || *q.BalanceUnit != monitor.Credits) {
				t.Errorf("credits quota shape wrong: %+v", q)
			}
		})
	}
}

func TestParseEmptyAndInvalid(t *testing.T) {
	quotas, tier, err := parseUsage([]byte("{}"), http.Header{}, testNow)
	if err != nil {
		t.Fatal(err)
	}
	if len(quotas) != 0 || tier != nil {
		t.Errorf("empty response: quotas=%+v tier=%+v", quotas, tier)
	}
	if _, _, err := parseUsage([]byte("not json"), http.Header{}, testNow); err == nil {
		t.Error("expected a parse error for invalid JSON")
	}
}

func TestPlanTypeTier(t *testing.T) {
	_, tier, err := parseUsage([]byte(`{"plan_type": "plus"}`), http.Header{}, testNow)
	if err != nil {
		t.Fatal(err)
	}
	if tier == nil || tier.Badge != "PLUS" {
		t.Errorf("tier = %+v, want PLUS", tier)
	}
	_, tier, err = parseUsage([]byte(`{}`), http.Header{}, testNow)
	if err != nil {
		t.Fatal(err)
	}
	if tier != nil {
		t.Errorf("tier = %+v, want nil", tier)
	}
}

func TestAuthPath(t *testing.T) {
	p := New(Config{HomeDir: "/fake/home", LookupEnv: noEnv})
	if got := p.AuthPath(); got != "/fake/home/.codex/auth.json" {
		t.Errorf("AuthPath = %q", got)
	}
	p = New(Config{LookupEnv: func(k string) string {
		if k == "CODEX_HOME" {
			return "/custom/codex"
		}
		return ""
	}})
	if got := p.AuthPath(); got != "/custom/codex/auth.json" {
		t.Errorf("AuthPath with CODEX_HOME = %q", got)
	}
}

func TestAvailable(t *testing.T) {
	dir := t.TempDir()
	p := newTestProvider(dir, Config{})
	if p.Available() {
		t.Error("Available with no auth file, want false")
	}
	writeAuth(t, dir, authJSON("tok", "ref", "", freshLast()))
	if !p.Available() {
		t.Error("Available with an auth file, want true")
	}
}

func TestLoadCredentials(t *testing.T) {
	dir := t.TempDir()
	p := newTestProvider(dir, Config{})
	if p.loadCredentials() != nil {
		t.Error("missing file should yield nil credentials")
	}
	writeAuth(t, dir, "not json")
	if p.loadCredentials() != nil {
		t.Error("malformed JSON should yield nil credentials")
	}
	writeAuth(t, dir, `{"someOtherKey": "value"}`)
	if c := p.loadCredentials(); c != nil {
		t.Error("missing tokens should yield nil credentials")
	}
	writeAuth(t, dir, authJSON("", "ref", "", ""))
	if c := p.loadCredentials(); c != nil {
		t.Error("empty access token should yield nil credentials")
	}
	writeAuth(t, dir, authJSON("my-access", "my-refresh", "acc-123", freshLast()))
	c := p.loadCredentials()
	if c == nil {
		t.Fatalf("load = %+v", c)
	}
	if c.accessToken != "my-access" || c.refreshToken != "my-refresh" || c.accountID != "acc-123" {
		t.Errorf("credentials = %+v", c)
	}
}

func TestNeedsRefresh(t *testing.T) {
	future := testNow.Add(time.Hour)
	past := testNow.Add(-time.Hour)
	soon := testNow.Add(4 * time.Minute)
	cases := []struct {
		name  string
		token string
		last  string
		want  bool
	}{
		{"jwt far future wins over old last", fakeJWT(&future), oldLast(), false},
		{"jwt expired", fakeJWT(&past), freshLast(), true},
		{"jwt within window", fakeJWT(&soon), freshLast(), true},
		{"no exp old last", "test-access-token", oldLast(), true},
		{"no exp recent last", "test-access-token", freshLast(), false},
		{"no exp missing last", "test-access-token", "", true},
		{"no exp garbage last", "test-access-token", "yesterday", true},
		{"malformed jwt old last", "a.b", oldLast(), true},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := needsRefresh(tc.token, tc.last, testNow); got != tc.want {
				t.Errorf("needsRefresh = %v, want %v", got, tc.want)
			}
		})
	}
}

func TestFetchSuccessSendsAuthHeaders(t *testing.T) {
	dir := t.TempDir()
	writeAuth(t, dir, authJSON("fresh-token", "ref", "acc-fake-1", freshLast()))

	var gotAuth, gotAccount, gotUA string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotAuth, gotAccount, gotUA = r.Header.Get("Authorization"), r.Header.Get("ChatGPT-Account-Id"), r.Header.Get("User-Agent")
		w.Write([]byte(`{"rate_limit": {"primary_window": {"used_percent": 10}}, "plan_type": "team"}`))
	}))
	defer srv.Close()

	p := newTestProvider(dir, Config{UsageURL: srv.URL, Client: srv.Client()})
	snap, err := p.Fetch(t.Context())
	if err != nil {
		t.Fatal(err)
	}
	if gotAuth != "Bearer fresh-token" || gotAccount != "acc-fake-1" || gotUA != "OpenUsage" {
		t.Errorf("headers: auth=%q account=%q ua=%q", gotAuth, gotAccount, gotUA)
	}
	if snap.ProviderID != "codex" {
		t.Errorf("provider = %q", snap.ProviderID)
	}
	if q := findQuota(snap.Quotas, "session"); q == nil || *q.PercentRemaining != 90 {
		t.Errorf("session quota = %+v, want 90", q)
	}
	if snap.Tier == nil || snap.Tier.Badge != "TEAM" {
		t.Errorf("tier = %+v, want TEAM", snap.Tier)
	}
}

func TestFetchRefreshesStaleTokenAndPreservesFields(t *testing.T) {
	dir := t.TempDir()
	writeAuth(t, dir, `{"tokens": {"access_token": "old-token", "refresh_token": "old-refresh",
		"account_id": "acc-fake-1", "id_token": "old-id", "custom_field": "keep-me"},
		"last_refresh": "`+oldLast()+`", "extra_top": {"nested": true}}`)

	refreshSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if ct := r.Header.Get("Content-Type"); ct != "application/x-www-form-urlencoded" {
			t.Errorf("refresh content type = %q", ct)
		}
		w.Write([]byte(`{"access_token": "new-token", "refresh_token": "new-refresh", "id_token": "new-id"}`))
	}))
	defer refreshSrv.Close()

	var usageAuth []string
	usageSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		usageAuth = append(usageAuth, r.Header.Get("Authorization"))
		w.Write([]byte(`{"rate_limit": {"primary_window": {"used_percent": 10}}}`))
	}))
	defer usageSrv.Close()

	p := newTestProvider(dir,
		Config{UsageURL: usageSrv.URL, RefreshURL: refreshSrv.URL,
			Client: usageSrv.Client()})
	snap, err := p.Fetch(t.Context())
	if err != nil {
		t.Fatal(err)
	}
	if len(usageAuth) != 1 || usageAuth[0] != "Bearer new-token" {
		t.Errorf("usage auth headers = %v, want one call with the new token", usageAuth)
	}
	if q := findQuota(snap.Quotas, "session"); q == nil || *q.PercentRemaining != 90 {
		t.Errorf("session quota = %+v", q)
	}

	data, err := os.ReadFile(filepath.Join(dir, ".codex", "auth.json"))
	if err != nil {
		t.Fatal(err)
	}
	var doc map[string]any
	if err := json.Unmarshal(data, &doc); err != nil {
		t.Fatal(err)
	}
	tokens, _ := doc["tokens"].(map[string]any)
	if tokens["access_token"] != "new-token" || tokens["refresh_token"] != "new-refresh" ||
		tokens["id_token"] != "new-id" || tokens["account_id"] != "acc-fake-1" ||
		tokens["custom_field"] != "keep-me" {
		t.Errorf("tokens not merged correctly: %v", tokens)
	}
	if _, ok := doc["extra_top"]; !ok {
		t.Errorf("top-level fields not preserved: %v", doc)
	}
	if _, ok := doc["last_refresh"].(string); !ok {
		t.Errorf("last_refresh not updated: %v", doc)
	}
}

func TestFetchUsesCurrentFileToken(t *testing.T) {
	dir := t.TempDir()
	// The file already holds the CLI-rotated token; the fetch must use it
	// and skip the refresh endpoint entirely.
	writeAuth(t, dir, authJSON("cli-rotated-token", "cli-refresh", "acc-fake-1", freshLast()))

	refreshHits := 0
	refreshSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		refreshHits++
		w.Write([]byte(`{"access_token": "should-not-be-used"}`))
	}))
	defer refreshSrv.Close()

	var usageAuth string
	usageSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		usageAuth = r.Header.Get("Authorization")
		w.Write([]byte(`{"rate_limit": {"primary_window": {"used_percent": 10}}}`))
	}))
	defer usageSrv.Close()

	p := newTestProvider(dir,
		Config{UsageURL: usageSrv.URL, RefreshURL: refreshSrv.URL,
			Client: usageSrv.Client()})
	if _, err := p.Fetch(t.Context()); err != nil {
		t.Fatal(err)
	}
	if usageAuth != "Bearer cli-rotated-token" || refreshHits != 0 {
		t.Errorf("auth=%q refreshHits=%d, want rotated token with no refresh", usageAuth, refreshHits)
	}
}

func TestFetch401RefreshesOnceThenRetries(t *testing.T) {
	dir := t.TempDir()
	writeAuth(t, dir, authJSON("stale-token", "good-refresh", "", freshLast()))

	refreshSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Write([]byte(`{"access_token": "retried-token"}`))
	}))
	defer refreshSrv.Close()

	calls := 0
	usageSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		calls++
		if r.Header.Get("Authorization") == "Bearer stale-token" {
			w.WriteHeader(http.StatusUnauthorized)
			return
		}
		w.Write([]byte(`{"rate_limit": {"primary_window": {"used_percent": 20}}}`))
	}))
	defer usageSrv.Close()

	p := newTestProvider(dir,
		Config{UsageURL: usageSrv.URL, RefreshURL: refreshSrv.URL,
			Client: usageSrv.Client()})
	snap, err := p.Fetch(t.Context())
	if err != nil {
		t.Fatal(err)
	}
	if calls != 2 {
		t.Errorf("usage calls = %d, want 2 (401 then retry)", calls)
	}
	if q := findQuota(snap.Quotas, "session"); q == nil || *q.PercentRemaining != 80 {
		t.Errorf("session quota = %+v, want 80", q)
	}
}

func TestFetchExpiredRefreshNeedsLogin(t *testing.T) {
	dir := t.TempDir()
	writeAuth(t, dir, authJSON("stale-token", "dead-refresh", "", freshLast()))

	refreshSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusBadRequest)
		w.Write([]byte(`{"error": {"code": "refresh_token_expired"}}`))
	}))
	defer refreshSrv.Close()

	usageSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusUnauthorized)
	}))
	defer usageSrv.Close()

	p := newTestProvider(dir,
		Config{UsageURL: usageSrv.URL, RefreshURL: refreshSrv.URL,
			Client: usageSrv.Client()})
	_, err := p.Fetch(t.Context())
	if err == nil || !strings.Contains(err.Error(), "log in again") {
		t.Errorf("err = %v, want a log-in-again error", err)
	}
}

func TestFetchRateLimitedHonoursRetryAfter(t *testing.T) {
	dir := t.TempDir()
	writeAuth(t, dir, authJSON("tok", "ref", "", freshLast()))

	usageSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Retry-After", "120")
		w.WriteHeader(http.StatusTooManyRequests)
	}))
	defer usageSrv.Close()

	p := newTestProvider(dir, Config{UsageURL: usageSrv.URL, Client: usageSrv.Client()})
	_, err := p.Fetch(t.Context())
	rl, ok := err.(*monitor.RateLimitedError)
	if !ok {
		t.Fatalf("err = %T %v, want *RateLimitedError", err, err)
	}
	if want := testNow.Add(2 * time.Minute); !rl.RetryAt.Equal(want) {
		t.Errorf("RetryAt = %v, want %v", rl.RetryAt, want)
	}
}

func TestFetchRateLimitedDefaultsWithoutHeader(t *testing.T) {
	dir := t.TempDir()
	writeAuth(t, dir, authJSON("tok", "ref", "", freshLast()))

	usageSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusTooManyRequests)
	}))
	defer usageSrv.Close()

	p := newTestProvider(dir, Config{UsageURL: usageSrv.URL, Client: usageSrv.Client()})
	_, err := p.Fetch(t.Context())
	rl, ok := err.(*monitor.RateLimitedError)
	if !ok {
		t.Fatalf("err = %T %v, want *RateLimitedError", err, err)
	}
	if want := testNow.Add(5 * time.Minute); !rl.RetryAt.Equal(want) {
		t.Errorf("RetryAt = %v, want %v", rl.RetryAt, want)
	}
}

func TestFetchErrorsTellUserToLogIn(t *testing.T) {
	dir := t.TempDir()
	p := newTestProvider(dir, Config{})

	_, err := p.Fetch(t.Context())
	if err == nil || !strings.Contains(err.Error(), "log in again") {
		t.Errorf("no-credentials err = %v, want log-in-again", err)
	}

	writeAuth(t, dir, authJSON("tok", "", "", freshLast()))
	usageSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusForbidden)
	}))
	defer usageSrv.Close()
	p = newTestProvider(dir, Config{UsageURL: usageSrv.URL, Client: usageSrv.Client()})
	_, err = p.Fetch(t.Context())
	if err == nil || !strings.Contains(err.Error(), "log in again") {
		t.Errorf("403 err = %v, want log-in-again", err)
	}
}

func TestFetchErrorsNeverContainSecrets(t *testing.T) {
	dir := t.TempDir()
	const secretAccess, secretRefresh = "access-secret-abc", "refresh-secret-xyz"
	writeAuth(t, dir, authJSON(secretAccess, secretRefresh, "", freshLast()))

	usageSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusInternalServerError)
	}))
	defer usageSrv.Close()

	p := newTestProvider(dir, Config{UsageURL: usageSrv.URL, Client: usageSrv.Client()})
	_, err := p.Fetch(t.Context())
	if err == nil {
		t.Fatal("expected an error for HTTP 500")
	}
	if strings.Contains(err.Error(), secretAccess) || strings.Contains(err.Error(), secretRefresh) {
		t.Errorf("error leaks a secret: %v", err)
	}
}
