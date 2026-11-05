package feed

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"quotad/monitor"
)

func testServer(now time.Time, providers ...monitor.Provider) *Server {
	m := monitor.New(providers,
		monitor.WithClock(fixedClock{now}),
		monitor.WithDisplayName(func(id string) string { return "Name-" + id }))
	s := &Server{Monitor: m, Now: func() time.Time { return now }}
	s.Hooks = NewHooks(func() Feed { return Build(m, now) }, s.Now)
	return s
}

func doRequest(t *testing.T, s *Server, method, path, host, body string) *httptest.ResponseRecorder {
	t.Helper()
	var reader *strings.Reader
	if body != "" {
		reader = strings.NewReader(body)
	} else {
		reader = strings.NewReader("")
	}
	r := httptest.NewRequest(method, path, reader)
	if host == "" {
		r.Host = ""
		r.Header.Del("Host")
	} else {
		r.Host = host
	}
	w := httptest.NewRecorder()
	s.ServeHTTP(w, r)
	return w
}

func TestQuotasReturnsDecodableFeed(t *testing.T) {
	now := time.Now()
	p := &staticProvider{id: "claude", snapshot: monitor.Snapshot{
		ProviderID: "claude",
		Quotas:     []monitor.Quota{monitor.NewPercentQuota(68, monitor.SessionType(), "claude")},
		CapturedAt: now,
	}}
	s := testServer(now, p)
	w := doRequest(t, s, "GET", "/quotas", "127.0.0.1", "")
	if w.Code != 200 {
		t.Fatalf("status = %d", w.Code)
	}
	var feed Feed
	if err := json.Unmarshal(w.Body.Bytes(), &feed); err != nil {
		t.Fatalf("response is not a feed: %v", err)
	}
	if len(feed.Providers) != 1 || feed.Providers[0].ID != "claude" {
		t.Errorf("providers = %+v", feed.Providers)
	}
}

func TestNonLoopbackHostRejected(t *testing.T) {
	now := time.Now()
	s := testServer(now)
	for _, host := range []string{"evil.example:8787", "evil.example", "192.168.1.2:8787", ""} {
		w := doRequest(t, s, "GET", "/quotas", host, "")
		if w.Code != http.StatusForbidden {
			t.Errorf("host %q: status = %d, want 403", host, w.Code)
		}
	}
}

func TestLoopbackHostVariantsAccepted(t *testing.T) {
	now := time.Now()
	s := testServer(now)
	for _, host := range []string{"127.0.0.1", "127.0.0.1:8787", "localhost", "localhost:9999"} {
		w := doRequest(t, s, "GET", "/quotas", host, "")
		if w.Code != 200 {
			t.Errorf("host %q: status = %d, want 200", host, w.Code)
		}
	}
}

func TestUnknownPathAndWrongMethod(t *testing.T) {
	now := time.Now()
	s := testServer(now)
	if w := doRequest(t, s, "GET", "/unknown", "localhost", ""); w.Code != 404 {
		t.Errorf("unknown path = %d, want 404", w.Code)
	}
	if w := doRequest(t, s, "POST", "/quotas", "localhost", ""); w.Code != 405 {
		t.Errorf("POST /quotas = %d, want 405", w.Code)
	}
	if w := doRequest(t, s, "GET", "/hooks/prompt", "localhost", ""); w.Code != 405 {
		t.Errorf("GET /hooks/prompt = %d, want 405", w.Code)
	}
	if w := doRequest(t, s, "PATCH", "/quotas", "localhost", ""); w.Code != 405 {
		t.Errorf("PATCH /quotas = %d, want 405", w.Code)
	}
}

func TestHooksDoNotRefresh(t *testing.T) {
	now := time.Now()
	p := &staticProvider{id: "codex", snapshot: monitor.Snapshot{
		ProviderID: "codex",
		Quotas:     []monitor.Quota{monitor.NewPercentQuota(81, monitor.WeeklyType(), "codex")},
		CapturedAt: now,
	}}
	s := testServer(now, p)
	body := `{"session_id":"s1"}`
	w := doRequest(t, s, "POST", "/hooks/session-start", "localhost", body)
	if w.Code != 200 {
		t.Fatalf("status = %d", w.Code)
	}
	w = doRequest(t, s, "POST", "/hooks/prompt", "localhost", body)
	if w.Code != 200 {
		t.Fatalf("status = %d", w.Code)
	}
	if p.fetches != 0 {
		t.Errorf("hooks must never refresh, fetches = %d", p.fetches)
	}
}

func TestQuotasRefreshesThroughMonitor(t *testing.T) {
	now := time.Now()
	p := &staticProvider{id: "codex", snapshot: monitor.Snapshot{
		ProviderID: "codex",
		Quotas:     []monitor.Quota{monitor.NewPercentQuota(81, monitor.WeeklyType(), "codex")},
		CapturedAt: now,
	}}
	s := testServer(now, p)
	w := doRequest(t, s, "GET", "/quotas", "localhost", "")
	if w.Code != 200 {
		t.Fatalf("status = %d", w.Code)
	}
	if p.fetches != 1 {
		t.Errorf("GET /quotas should refresh once, fetches = %d", p.fetches)
	}
	var feed Feed
	if err := json.Unmarshal(w.Body.Bytes(), &feed); err != nil {
		t.Fatal(err)
	}
	if len(feed.Providers) != 1 {
		t.Errorf("providers = %+v", feed.Providers)
	}
}

func TestLiveHTTPRoundTrip(t *testing.T) {
	now := time.Now()
	p := &staticProvider{id: "claude", snapshot: monitor.Snapshot{
		ProviderID: "claude",
		Quotas:     []monitor.Quota{monitor.NewPercentQuota(68, monitor.SessionType(), "claude")},
		CapturedAt: now,
	}}
	s := testServer(now, p)
	server := httptest.NewServer(s)
	defer server.Close()
	resp, err := http.Get(server.URL + "/quotas") // httptest binds 127.0.0.1
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		t.Fatalf("status = %d", resp.StatusCode)
	}
	if ct := resp.Header.Get("Content-Type"); ct != "application/json" {
		t.Errorf("content type = %q", ct)
	}
	data, _ := io.ReadAll(resp.Body)
	var feed Feed
	if err := json.Unmarshal(data, &feed); err != nil {
		t.Fatalf("not a feed: %v", err)
	}
}

func TestServerBindsLoopback(t *testing.T) {
	s := testServer(time.Now())
	if err := s.Start(0); err != nil {
		t.Skipf("cannot bind loopback in this environment: %v", err)
	}
	defer s.Stop(context.Background())
}
