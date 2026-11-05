package opencode

import (
	"context"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"quotad/monitor"
)

// fileProvider returns a Provider whose key comes from a temp auth.json.
func fileProvider(t *testing.T, key string) *Provider {
	t.Helper()
	home := t.TempDir()
	writeAuthFile(t, filepath.Join(home, ".local", "share"), key)
	return New(WithHome(home), WithLookupEnv(testEnv()))
}

func withServer(t *testing.T, handle http.HandlerFunc) (*Provider, *int) {
	t.Helper()
	var hits int
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits++
		handle(w, r)
	}))
	t.Cleanup(server.Close)
	p := fileProvider(t, "sk-test-key")
	p.baseURL = server.URL
	return p, &hits
}

func TestParseUsageResponse(t *testing.T) {
	body, err := os.ReadFile("testdata/usage.json")
	if err != nil {
		t.Fatal(err)
	}
	quotas, err := parseUsageResponse(body)
	if err != nil {
		t.Fatal(err)
	}
	if len(quotas) != 3 {
		t.Fatalf("got %d quotas", len(quotas))
	}
	for _, q := range quotas {
		if q.ProviderID != ProviderID {
			t.Fatalf("wrong provider id %q", q.ProviderID)
		}
	}
	if got := *quotas[0].PercentRemaining; got < 79.166 || got > 79.167 {
		t.Fatalf("session remaining: got %v", got)
	}
	if quotas[0].Type != monitor.SessionType() {
		t.Fatalf("first window should be session: %+v", quotas[0].Type)
	}
	if quotas[0].ResetsAt == nil {
		t.Fatal("session reset should be set")
	}
	if got := *quotas[1].PercentRemaining; got != 75 {
		t.Fatalf("weekly remaining: got %v", got)
	}
	if quotas[1].Type != monitor.WeeklyType() {
		t.Fatalf("second window should be weekly: %+v", quotas[1].Type)
	}
	if got := *quotas[2].PercentRemaining; got != 75 {
		t.Fatalf("monthly remaining: got %v", got)
	}
	if quotas[2].Type != monitor.TimeLimitType("Monthly") {
		t.Fatalf("third window should be Monthly: %+v", quotas[2].Type)
	}
}

func TestParseZeroUsage(t *testing.T) {
	body := []byte(`{"usage":{
		"rolling":{"status":"ok","percent":0,"resetsAt":null},
		"weekly":{"status":"ok","percent":0,"resetsAt":null},
		"monthly":{"status":"ok","percent":0,"resetsAt":null}}}`)
	quotas, err := parseUsageResponse(body)
	if err != nil {
		t.Fatal(err)
	}
	for _, q := range quotas {
		if *q.PercentRemaining != 100 || q.ResetsAt != nil {
			t.Fatalf("zero usage should be 100 with no reset: %+v", q)
		}
	}
}

func TestParseClampsOverLimit(t *testing.T) {
	body := []byte(`{"usage":{
		"rolling":{"status":"exceeded","percent":140,"resetsAt":"2026-09-01T22:38:55.674Z"},
		"weekly":{"status":"ok","percent":50,"resetsAt":"2026-09-07T00:00:00.000Z"},
		"monthly":{"status":"ok","percent":50,"resetsAt":"2026-09-07T00:00:00.000Z"}}}`)
	quotas, err := parseUsageResponse(body)
	if err != nil {
		t.Fatal(err)
	}
	if got := *quotas[0].PercentRemaining; got != 0 {
		t.Fatalf("over-limit usage should clamp to 0: got %v", got)
	}
}

func TestParsePartialResponseFails(t *testing.T) {
	body := []byte(`{"usage":{"rolling":{"percent":10}}}`)
	if _, err := parseUsageResponse(body); err == nil {
		t.Fatal("partial response should fail, not return partial quotas")
	}
}

func TestParseMalformedJSONFails(t *testing.T) {
	if _, err := parseUsageResponse([]byte("not json")); err == nil {
		t.Fatal("malformed JSON should fail")
	}
}

func TestPercentRemaining(t *testing.T) {
	cases := []struct {
		used, want float64
	}{
		{0, 100},
		{25, 75},
		{140, 0},
		{-5, 100},
	}
	for _, c := range cases {
		if got := percentRemaining(c.used); got != c.want {
			t.Fatalf("percentRemaining(%v) = %v, want %v", c.used, got, c.want)
		}
	}
}

func TestParseResetTimeForms(t *testing.T) {
	frac := "2026-09-01T22:38:55.674Z"
	if parseResetTime(&frac) == nil {
		t.Fatal("fractional ISO 8601 should parse")
	}
	plain := "2026-09-01T22:38:55Z"
	if parseResetTime(&plain) == nil {
		t.Fatal("plain ISO 8601 should parse")
	}
	if parseResetTime(nil) != nil {
		t.Fatal("nil should give nil")
	}
	empty := ""
	if parseResetTime(&empty) != nil {
		t.Fatal("empty should give nil")
	}
	bad := "yesterday"
	if parseResetTime(&bad) != nil {
		t.Fatal("unparseable should give nil, not an error")
	}
}

func TestFetchSuccess(t *testing.T) {
	p, hits := withServer(t, func(w http.ResponseWriter, r *http.Request) {
		if got := r.Header.Get("Authorization"); got != "Bearer sk-test-key" {
			t.Errorf("wrong Authorization header: %q", got)
		}
		if got := r.Header.Get("Accept"); got != "application/json" {
			t.Errorf("wrong Accept header: %q", got)
		}
		body, _ := os.ReadFile("testdata/usage.json")
		w.Write(body)
	})
	snap, err := p.Fetch(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if *hits != 1 {
		t.Fatalf("expected one request, got %d", *hits)
	}
	if snap.ProviderID != ProviderID || len(snap.Quotas) != 3 {
		t.Fatalf("bad snapshot: %+v", snap)
	}
}

func TestFetchWithoutCredentialsMakesNoRequest(t *testing.T) {
	var hits int
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits++
	}))
	t.Cleanup(server.Close)
	p := New(WithHome(t.TempDir()), WithLookupEnv(testEnv()), WithBaseURL(server.URL))
	if _, err := p.Fetch(context.Background()); err == nil {
		t.Fatal("missing key should fail")
	} else if !strings.Contains(err.Error(), "log in") {
		t.Fatalf("error should tell the user to log in: %v", err)
	}
	if hits != 0 {
		t.Fatal("no credentials: must not send a request")
	}
}

func TestFetchUnauthorized(t *testing.T) {
	for _, code := range []int{http.StatusUnauthorized, http.StatusForbidden} {
		p, _ := withServer(t, func(w http.ResponseWriter, r *http.Request) {
			w.WriteHeader(code)
		})
		if _, err := p.Fetch(context.Background()); err == nil {
			t.Fatalf("status %d should fail", code)
		} else if !strings.Contains(err.Error(), "log in") {
			t.Fatalf("status %d: error should tell the user to log in: %v", code, err)
		}
	}
}

func TestFetchRateLimitedHonorsRetryAfter(t *testing.T) {
	p, _ := withServer(t, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Retry-After", "120")
		w.WriteHeader(http.StatusTooManyRequests)
	})
	before := time.Now()
	_, err := p.Fetch(context.Background())
	rl, ok := err.(*monitor.RateLimitedError)
	if !ok {
		t.Fatalf("expected RateLimitedError, got %T (%v)", err, err)
	}
	if want := before.Add(120 * time.Second); rl.RetryAt.Before(want.Add(-10*time.Second)) ||
		rl.RetryAt.After(want.Add(10*time.Second)) {
		t.Fatalf("RetryAt %v should be ~120s out", rl.RetryAt)
	}
}

func TestFetchRateLimitedDefaultsWithoutHeader(t *testing.T) {
	p, _ := withServer(t, func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusTooManyRequests)
	})
	before := time.Now()
	_, err := p.Fetch(context.Background())
	rl, ok := err.(*monitor.RateLimitedError)
	if !ok {
		t.Fatalf("expected RateLimitedError, got %T (%v)", err, err)
	}
	if want := before.Add(defaultRateDelay); rl.RetryAt.Before(want.Add(-10*time.Second)) ||
		rl.RetryAt.After(want.Add(10*time.Second)) {
		t.Fatalf("RetryAt %v should fall back to the default delay", rl.RetryAt)
	}
}

func TestFetchRateLimitedHTTPDate(t *testing.T) {
	p, _ := withServer(t, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Retry-After", time.Now().Add(90*time.Second).UTC().Format(http.TimeFormat))
		w.WriteHeader(http.StatusTooManyRequests)
	})
	_, err := p.Fetch(context.Background())
	rl, ok := err.(*monitor.RateLimitedError)
	if !ok {
		t.Fatalf("expected RateLimitedError, got %T (%v)", err, err)
	}
	if d := time.Until(rl.RetryAt); d < 60*time.Second || d > 120*time.Second {
		t.Fatalf("RetryAt should honor the HTTP date, got %v out", d)
	}
}

func TestFetchServerError(t *testing.T) {
	p, _ := withServer(t, func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusInternalServerError)
	})
	if _, err := p.Fetch(context.Background()); err == nil {
		t.Fatal("status 500 should fail")
	}
}

func TestFetchMalformedBody(t *testing.T) {
	p, _ := withServer(t, func(w http.ResponseWriter, r *http.Request) {
		w.Write([]byte("not json"))
	})
	if _, err := p.Fetch(context.Background()); err == nil {
		t.Fatal("malformed body should fail")
	}
}

func TestFetchErrorNeverCarriesKey(t *testing.T) {
	p, _ := withServer(t, func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusInternalServerError)
	})
	_, err := p.Fetch(context.Background())
	if err == nil {
		t.Fatal("expected an error")
	}
	if strings.Contains(err.Error(), "sk-test-key") {
		t.Fatalf("error must not carry the key: %v", err)
	}
}
