package feed

import (
	"context"
	"encoding/json"
	"os"
	"reflect"
	"testing"
	"time"

	"quotad/monitor"
)

func f64(v float64) *float64 { return &v }
func iptr(v int) *int        { return &v }

func timePtr(s string) *time.Time {
	t, err := time.Parse(time.RFC3339, s)
	if err != nil {
		panic(err)
	}
	return &t
}

// goldenProviders rebuilds the providers captured in swift-quotas.json
// through the Go monitor types.
func goldenProviders() ([]monitor.Provider, map[string]string) {
	pq := func(remaining float64, typ monitor.QuotaType, resets string) monitor.Quota {
		q := monitor.NewPercentQuota(remaining, typ, "")
		q.ResetsAt = timePtr(resets)
		return q
	}
	mk := func(id string, tier *monitor.Tier, captured string, quotas ...monitor.Quota) monitor.Provider {
		for i := range quotas {
			quotas[i].ProviderID = id
		}
		cap, _ := time.Parse(time.RFC3339, captured)
		return &staticProvider{id: id, snapshot: monitor.Snapshot{
			ProviderID: id, Quotas: quotas, CapturedAt: cap, Tier: tier,
		}}
	}
	maxTier := monitor.ClaudeMaxTier()
	claude := mk("claude", &maxTier, "2026-11-05T22:04:18Z",
		pq(91, monitor.WeeklyType(), "2026-11-12T03:00:00Z"),
		pq(100, monitor.ModelType("fable"), "2026-11-12T03:00:00Z"))
	prolite := monitor.CustomTier("PROLITE")
	codex := mk("codex", &prolite, "2026-11-05T22:08:00Z",
		pq(77, monitor.SessionType(), "2026-11-11T06:35:45Z"))
	pro := monitor.CustomTier("PRO")
	cursorQ := monitor.NewPercentQuota(45.089405684754524, monitor.TimeLimitType("Monthly"), "cursor")
	cursorQ.ResetsAt = timePtr("2026-11-29T23:58:32Z")
	cursorQ.UnitsUsed = iptr(14586)
	cursorQ.UnitsLimit = iptr(26563)
	cursor := mk("cursor", &pro, "2026-11-05T22:08:00Z", cursorQ)
	opencode := mk("opencode-go", nil, "2026-11-05T22:08:00Z",
		pq(100, monitor.SessionType(), "2026-11-06T03:08:00Z"),
		pq(100, monitor.WeeklyType(), "2026-11-09T00:00:00Z"),
		pq(0, monitor.TimeLimitType("Monthly"), "2026-11-12T22:13:11Z"))
	power := monitor.CustomTier("Muse Code Power Usage")
	muse := mk("muse", &power, "2026-11-05T22:04:18Z",
		pq(86, monitor.SessionType(), "2026-11-06T00:24:17Z"),
		pq(44, monitor.WeeklyType(), "2026-11-09T00:00:00Z"))
	names := map[string]string{
		"claude": "Claude", "codex": "Codex", "cursor": "Cursor",
		"opencode-go": "OpenCode Go", "muse": "Muse",
	}
	return []monitor.Provider{claude, codex, cursor, opencode, muse}, names
}

func goldenTime() time.Time {
	t, _ := time.Parse(time.RFC3339, "2026-11-05T22:08:10Z")
	return t
}

func goldenMonitor(t *testing.T, now time.Time) *monitor.Monitor {
	t.Helper()
	providers, names := goldenProviders()
	m := monitor.New(providers,
		monitor.WithClock(fixedClock{now}),
		monitor.WithDisplayName(func(id string) string { return names[id] }))
	m.Refresh(context.Background(), true)
	return m
}

// stripVolatile removes generatedAt everywhere and ageSeconds, which depend
// on when the feed was captured rather than on the mapping.
func stripVolatile(v any) {
	switch n := v.(type) {
	case map[string]any:
		delete(n, "generatedAt")
		delete(n, "ageSeconds")
		for _, child := range n {
			stripVolatile(child)
		}
	case []any:
		for _, child := range n {
			stripVolatile(child)
		}
	}
}

func TestGoldenQuotasMatchSwift(t *testing.T) {
	raw, err := os.ReadFile("testdata/swift-quotas.json")
	if err != nil {
		t.Fatal(err)
	}
	now := goldenTime()
	m := goldenMonitor(t, now)
	data, err := json.Marshal(Build(m, now))
	if err != nil {
		t.Fatal(err)
	}
	var want, got any
	if err := json.Unmarshal(raw, &want); err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(data, &got); err != nil {
		t.Fatal(err)
	}
	stripVolatile(want)
	stripVolatile(got)
	if !reflect.DeepEqual(got, want) {
		gotJSON, _ := json.MarshalIndent(got, "", "  ")
		wantJSON, _ := json.MarshalIndent(want, "", "  ")
		t.Errorf("feed mismatch.\n got: %s\nwant: %s", gotJSON, wantJSON)
	}
}

func TestGoldenSessionStartMatchesSwift(t *testing.T) {
	raw, err := os.ReadFile("testdata/swift-session-start.json")
	if err != nil {
		t.Fatal(err)
	}
	now := goldenTime()
	m := goldenMonitor(t, now)
	hooks := NewHooks(func() Feed { return Build(m, now) }, func() time.Time { return now })
	got := hooks.SessionStart([]byte(`{"session_id":"s1","hook_event_name":"SessionStart"}`))
	var want, gotJSON any
	if err := json.Unmarshal(raw, &want); err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(got, &gotJSON); err != nil {
		t.Fatalf("session-start is not JSON: %v\n%s", err, got)
	}
	if !reflect.DeepEqual(gotJSON, want) {
		t.Errorf("session-start mismatch.\n got: %s\nwant: %s", got, raw)
	}
}

func TestAcceptanceFeedShape(t *testing.T) {
	captured := time.Unix(1_700_000_000, 0)
	now := captured.Add(122 * time.Second)
	claudeResets := captured.Add(time.Hour)
	codexWeekly := monitor.NewPercentQuota(81, monitor.WeeklyType(), "codex")
	maxTier := monitor.ClaudeMaxTier()
	plus := monitor.CustomTier("Plus")
	m := monitor.New([]monitor.Provider{
		&staticProvider{id: "claude", snapshot: monitor.Snapshot{
			ProviderID: "claude",
			Quotas: []monitor.Quota{
				{PercentRemaining: f64(68), Type: monitor.SessionType(), ProviderID: "claude", ResetsAt: &claudeResets},
				{PercentRemaining: f64(12), Type: monitor.WeeklyType(), ProviderID: "claude", ResetsAt: timePtr("2023-11-15T22:13:20Z")},
			},
			CapturedAt: captured, Tier: &maxTier,
		}},
		&staticProvider{id: "codex", snapshot: monitor.Snapshot{
			ProviderID: "codex", Quotas: []monitor.Quota{codexWeekly},
			CapturedAt: captured, Tier: &plus,
		}},
	},
		monitor.WithClock(fixedClock{now}),
		monitor.WithDisplayName(func(id string) string {
			if id == "claude" {
				return "Claude"
			}
			return "Codex"
		}))
	m.Refresh(context.Background(), true)
	feed := Build(m, now)
	if len(feed.Providers) != 2 {
		t.Fatalf("providers = %d, want 2", len(feed.Providers))
	}
	claude := feed.Providers[0]
	if claude.Tier == nil || *claude.Tier != "Max" {
		t.Errorf("claude tier = %v, want Max", claude.Tier)
	}
	if claude.AgeSeconds == nil || *claude.AgeSeconds != 122 {
		t.Errorf("claude age = %v, want 122", claude.AgeSeconds)
	}
	if claude.Quotas[0].Key != "session" {
		t.Errorf("first quota key = %q", claude.Quotas[0].Key)
	}
	// Decodability: the DTO must round-trip through JSON.
	raw, _ := json.Marshal(feed)
	var decoded Feed
	if err := json.Unmarshal(raw, &decoded); err != nil {
		t.Fatalf("feed does not round-trip: %v", err)
	}
}

func TestProviderStates(t *testing.T) {
	now := time.Now()
	retryAt := now.Add(time.Hour)
	throttledErr := &monitor.RateLimitedError{RetryAt: retryAt}
	newMonitor := func(p monitor.Provider) *monitor.Monitor {
		m := monitor.New([]monitor.Provider{p}, monitor.WithClock(fixedClock{now}))
		m.Refresh(context.Background(), true)
		return m
	}
	t.Run("rate limited maps to throttledUntil", func(t *testing.T) {
		m := newMonitor(&staticProvider{id: "claude", err: throttledErr})
		got := Build(m, now).Providers[0]
		if got.ThrottledUntil == nil {
			t.Fatal("throttledUntil should be set")
		}
		if got.Unavailable != nil {
			t.Error("unavailable must be nil when throttled")
		}
	})
	t.Run("plain error maps to unavailable", func(t *testing.T) {
		m := newMonitor(&staticProvider{id: "claude", err: errBoom})
		got := Build(m, now).Providers[0]
		if got.Unavailable == nil || *got.Unavailable != "boom" {
			t.Errorf("unavailable = %v", got.Unavailable)
		}
		if got.Status != "unknown" {
			t.Errorf("status = %q, want unknown", got.Status)
		}
	})
	t.Run("empty quotas are reported", func(t *testing.T) {
		m := newMonitor(&staticProvider{id: "claude", snapshot: monitor.Snapshot{
			ProviderID: "claude", CapturedAt: now,
		}})
		got := Build(m, now).Providers[0]
		if got.Unavailable == nil || *got.Unavailable != "No current quota data" {
			t.Errorf("unavailable = %v", got.Unavailable)
		}
	})
	t.Run("disabled providers are listed", func(t *testing.T) {
		p := &staticProvider{id: "claude"}
		m := monitor.New([]monitor.Provider{p},
			monitor.WithClock(fixedClock{now}),
			monitor.WithIsEnabled(func(string) bool { return false }))
		feed := Build(m, now)
		if len(feed.Providers) != 0 || len(feed.DisabledProviderIDs) != 1 {
			t.Errorf("disabled provider should be listed, got %+v", feed)
		}
		if feed.DisabledProviderIDs[0] != "claude" {
			t.Errorf("disabled ids = %v", feed.DisabledProviderIDs)
		}
	})
}
