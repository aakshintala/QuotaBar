package muse_test

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"

	"quotad/monitor"
	"quotad/providers/muse"
)

// asOf is just after the recorded usage-ok.json reading (asOfMs), before
// both of its resets.
var asOf = time.UnixMilli(1794257331168).UTC()

// stubCLI is a fake Muse CLI: it counts runs, records the last call and
// returns canned output.
type stubCLI struct {
	calls  int
	dir    string
	bin    string
	args   []string
	stdout []byte
	stderr []byte
	err    error
}

func (s *stubCLI) run(_ context.Context, dir, bin string, args ...string) ([]byte, []byte, error) {
	s.calls++
	s.dir, s.bin, s.args = dir, bin, args
	return s.stdout, s.stderr, s.err
}

func fixture(t *testing.T, name string) []byte {
	t.Helper()
	data, err := os.ReadFile(filepath.Join("testdata", name))
	if err != nil {
		t.Fatal(err)
	}
	return data
}

func env(home string, extra map[string]string) func(string) string {
	return func(k string) string {
		if k == "HOME" {
			return home
		}
		return extra[k]
	}
}

func noPath(string) (string, error) { return "", errors.New("not on PATH") }

// harness builds a provider over a temp HOME with a stubbed CLI resolved
// through $MUSE_BIN and a mutable clock.
type harness struct {
	home string
	now  time.Time
	cli  *stubCLI
}

func newHarness(t *testing.T, stdout []byte) *harness {
	t.Helper()
	return &harness{home: t.TempDir(), now: asOf, cli: &stubCLI{stdout: stdout}}
}

func (h *harness) provider() *muse.Provider {
	return muse.New(
		muse.WithNow(func() time.Time { return h.now }),
		muse.WithLookupEnv(env(h.home, map[string]string{"MUSE_BIN": "/opt/muse"})),
		muse.WithLookPath(noPath),
		muse.WithRunner(h.cli.run),
	)
}

func TestID(t *testing.T) {
	if got := muse.New().ID(); got != "muse" {
		t.Errorf("ID = %q", got)
	}
}

func TestHappyPathRunsUsageJSON(t *testing.T) {
	h := newHarness(t, fixture(t, "usage-ok.json"))
	snap, err := h.provider().Fetch(t.Context())
	if err != nil {
		t.Fatalf("Fetch: %v", err)
	}
	if h.cli.calls != 1 || h.cli.bin != "/opt/muse" || h.cli.dir != h.home ||
		strings.Join(h.cli.args, " ") != "usage --json" {
		t.Errorf("ran %d× %q %v in %q", h.cli.calls, h.cli.bin, h.cli.args, h.cli.dir)
	}
	if snap.ProviderID != "muse" || !snap.CapturedAt.Equal(asOf) {
		t.Errorf("snap = %+v", snap)
	}
	if snap.Tier == nil || snap.Tier.Badge != "Muse Code Power Usage" || snap.Tier.Claude {
		t.Errorf("Tier = %+v", snap.Tier)
	}
	if len(snap.Quotas) != 2 {
		t.Fatalf("quotas = %d, want 2", len(snap.Quotas))
	}
	session, weekly := snap.Quotas[0], snap.Quotas[1]
	if session.Type.Kind != monitor.KindSession || *session.PercentRemaining != 98 ||
		session.ResetsAt.UnixMilli() != 1794273003000 {
		t.Errorf("session = %+v (%v)", session, *session.PercentRemaining)
	}
	if weekly.Type.Kind != monitor.KindWeekly || *weekly.PercentRemaining != 80 ||
		weekly.ResetsAt.UnixMilli() != 1794787200000 {
		t.Errorf("weekly = %+v (%v)", weekly, *weekly.PercentRemaining)
	}
}

func TestMissingAndExpiredWindowsAreOmittedAndUsageClamped(t *testing.T) {
	h := newHarness(t, []byte(`{"schemaVersion":1,"allowance":{"tier":"T",
		"window":{"usedPercent":150,"resetsAtMs":1794273003000},"weekly":null},"reason":null,"hint":null}`))
	snap, err := h.provider().Fetch(t.Context())
	if err != nil {
		t.Fatalf("Fetch: %v", err)
	}
	if len(snap.Quotas) != 1 || *snap.Quotas[0].PercentRemaining != 0 {
		t.Fatalf("quotas = %+v", snap.Quotas)
	}

	h = newHarness(t, []byte(`{"schemaVersion":1,"allowance":{"tier":"T",
		"window":{"usedPercent":10,"resetsAtMs":1000},"weekly":{"usedPercent":-5}}}`))
	snap, err = h.provider().Fetch(t.Context())
	if err != nil {
		t.Fatalf("Fetch: %v", err)
	}
	if len(snap.Quotas) != 1 || snap.Quotas[0].Type.Kind != monitor.KindWeekly ||
		*snap.Quotas[0].PercentRemaining != 100 || snap.Quotas[0].ResetsAt != nil {
		t.Fatalf("quotas = %+v", snap.Quotas)
	}
}

func TestNoDataWithoutPriorReadingReportsHint(t *testing.T) {
	h := newHarness(t, fixture(t, "usage-no-data.json"))
	_, err := h.provider().Fetch(t.Context())
	if err == nil || !strings.Contains(err.Error(), "before your first use in a new window") {
		t.Fatalf("err = %v", err)
	}
}

func TestNoDataFallsBackToReasonWithoutHint(t *testing.T) {
	h := newHarness(t, []byte(`{"schemaVersion":1,"allowance":null,"reason":"server_sent_no_usage","hint":null}`))
	_, err := h.provider().Fetch(t.Context())
	if err == nil || !strings.Contains(err.Error(), "server_sent_no_usage") {
		t.Fatalf("err = %v", err)
	}
}

func TestNoDataWithPriorReadingInMemoryKeepsIt(t *testing.T) {
	h := newHarness(t, fixture(t, "usage-ok.json"))
	p := h.provider()
	if _, err := p.Fetch(t.Context()); err != nil {
		t.Fatal(err)
	}
	h.now = h.now.Add(cooldown())
	h.cli.stdout = fixture(t, "usage-no-data.json")
	_, err := p.Fetch(t.Context())
	if err == nil || !strings.Contains(err.Error(), "showing the last known reading") {
		t.Fatalf("err = %v", err)
	}
	// The persisted reading survives the quota-less reply.
	h.now = h.now.Add(time.Minute)
	if snap, err := h.provider().Fetch(t.Context()); err != nil || len(snap.Quotas) != 2 {
		t.Fatalf("restart inside cooldown: snap = %+v, err = %v", snap, err)
	}
}

func TestNoDataWithPriorReadingOnDiskKeepsIt(t *testing.T) {
	h := newHarness(t, fixture(t, "usage-ok.json"))
	if _, err := h.provider().Fetch(t.Context()); err != nil {
		t.Fatal(err)
	}
	h.now = h.now.Add(cooldown())
	h.cli.stdout = fixture(t, "usage-no-data.json")
	_, err := h.provider().Fetch(t.Context()) // a fresh provider: a restart
	if err == nil || !strings.Contains(err.Error(), "showing the last known reading") {
		t.Fatalf("err = %v", err)
	}
}

func cooldown() time.Duration { return 5*time.Minute + time.Second }

func TestCooldownSkipsTheCLIAndRepeatsTheReading(t *testing.T) {
	h := newHarness(t, fixture(t, "usage-ok.json"))
	p := h.provider()
	first, err := p.Fetch(t.Context())
	if err != nil {
		t.Fatal(err)
	}
	h.now = h.now.Add(4 * time.Minute)
	again, err := p.Fetch(t.Context())
	if err != nil || len(again.Quotas) != len(first.Quotas) {
		t.Fatalf("again = %+v, err = %v", again, err)
	}
	// A restart inside the cooldown serves the persisted reading.
	if _, err := h.provider().Fetch(t.Context()); err != nil {
		t.Fatalf("restart: %v", err)
	}
	if h.cli.calls != 1 {
		t.Errorf("CLI ran %d times, want 1", h.cli.calls)
	}
	h.now = h.now.Add(2 * time.Minute)
	if _, err := p.Fetch(t.Context()); err != nil {
		t.Fatal(err)
	}
	if h.cli.calls != 2 {
		t.Errorf("CLI ran %d times after the cooldown, want 2", h.cli.calls)
	}
}

func TestCooldownRepeatsLastErrorNotARateLimit(t *testing.T) {
	h := newHarness(t, nil)
	h.cli.err = errors.New("exit status 1")
	h.cli.stderr = []byte("  not logged in\n run `muse login`  ")
	p := h.provider()
	_, first := p.Fetch(t.Context())
	if first == nil || !strings.Contains(first.Error(), "not logged in run `muse login`") {
		t.Fatalf("first = %v", first)
	}
	h.now = h.now.Add(time.Minute)
	_, again := p.Fetch(t.Context())
	var rl *monitor.RateLimitedError
	if again == nil || again.Error() != first.Error() || errors.As(again, &rl) {
		t.Fatalf("again = %v, want the first error", again)
	}
	if h.cli.calls != 1 {
		t.Errorf("CLI ran %d times, want 1", h.cli.calls)
	}
}

func TestCooldownWithNothingToRepeatIsNotARateLimit(t *testing.T) {
	h := newHarness(t, nil)
	dir := filepath.Join(h.home, ".quotabar", "muse")
	if err := os.MkdirAll(dir, 0o700); err != nil {
		t.Fatal(err)
	}
	stamp := []byte(strconv.FormatInt(asOf.Unix()-30, 10))
	if err := os.WriteFile(filepath.Join(dir, "cli.request"), stamp, 0o600); err != nil {
		t.Fatal(err)
	}
	_, err := h.provider().Fetch(t.Context())
	var rl *monitor.RateLimitedError
	if err == nil || errors.As(err, &rl) || !strings.Contains(err.Error(), "at most every") {
		t.Fatalf("err = %v", err)
	}
	if h.cli.calls != 0 {
		t.Errorf("CLI ran %d times, want 0", h.cli.calls)
	}
}

func TestRunFailureAfterRestartServesPersistedReading(t *testing.T) {
	h := newHarness(t, fixture(t, "usage-ok.json"))
	if _, err := h.provider().Fetch(t.Context()); err != nil {
		t.Fatal(err)
	}
	h.now = h.now.Add(cooldown())
	h.cli.err = errors.New("exit status 1")
	snap, err := h.provider().Fetch(t.Context())
	if err != nil || len(snap.Quotas) != 2 {
		t.Fatalf("snap = %+v, err = %v", snap, err)
	}
}

func TestNonZeroExitIsAnErrorWithCappedStderr(t *testing.T) {
	h := newHarness(t, nil)
	h.cli.err = errors.New("exit status 2")
	h.cli.stderr = []byte(strings.Repeat("x", 1000))
	_, err := h.provider().Fetch(t.Context())
	if err == nil || !strings.Contains(err.Error(), "exit status 2") {
		t.Fatalf("err = %v", err)
	}
	if !strings.HasSuffix(err.Error(), ": "+strings.Repeat("x", 200)+"…") {
		t.Errorf("stderr excerpt not capped at 200 chars: %q", err)
	}
}

func TestUnparseableOutputIsAnError(t *testing.T) {
	h := newHarness(t, []byte("Usage: muse usage [--json]"))
	if _, err := h.provider().Fetch(t.Context()); err == nil || !strings.Contains(err.Error(), "could not be decoded") {
		t.Fatalf("err = %v", err)
	}
}

func TestUnsupportedSchemaIsAnError(t *testing.T) {
	h := newHarness(t, []byte(`{"schemaVersion":2,"allowance":null}`))
	if _, err := h.provider().Fetch(t.Context()); err == nil || !strings.Contains(err.Error(), "unsupported muse usage schema") {
		t.Fatalf("err = %v", err)
	}
}

func TestCacheHoldsOnlyQuotaData(t *testing.T) {
	h := newHarness(t, fixture(t, "usage-ok.json"))
	if _, err := h.provider().Fetch(t.Context()); err != nil {
		t.Fatal(err)
	}
	entries, err := os.ReadDir(filepath.Join(h.home, ".quotabar", "muse"))
	if err != nil {
		t.Fatal(err)
	}
	var names []string
	for _, e := range entries {
		names = append(names, e.Name())
	}
	if strings.Join(names, ",") != "cli.json,cli.request" {
		t.Errorf("cache files = %v", names)
	}
}

func TestBinaryResolutionOrder(t *testing.T) {
	home := t.TempDir()
	local := filepath.Join(home, ".local", "bin", "muse")
	onPath := func(string) (string, error) { return "/usr/local/bin/muse", nil }
	resolve := func(extra map[string]string, lookPath func(string) (string, error)) (string, bool) {
		cli := &stubCLI{stdout: fixture(t, "usage-ok.json")}
		p := muse.New(muse.WithNow(func() time.Time { return asOf }),
			muse.WithLookupEnv(env(home, extra)), muse.WithLookPath(lookPath), muse.WithRunner(cli.run))
		avail := p.Available()
		_ = os.RemoveAll(filepath.Join(home, ".quotabar")) // drop the cooldown
		_, _ = p.Fetch(t.Context())
		return cli.bin, avail
	}

	if bin, ok := resolve(nil, noPath); ok || bin != "" {
		t.Errorf("nothing installed: bin = %q, available = %v", bin, ok)
	}
	if bin, ok := resolve(nil, onPath); !ok || bin != "/usr/local/bin/muse" {
		t.Errorf("PATH only: bin = %q, available = %v", bin, ok)
	}
	if err := os.MkdirAll(filepath.Dir(local), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(local, []byte("#!/bin/sh\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	if bin, ok := resolve(nil, onPath); !ok || bin != local {
		t.Errorf("home before PATH: bin = %q, available = %v", bin, ok)
	}
	if bin, ok := resolve(map[string]string{"MUSE_BIN": "/opt/muse"}, onPath); !ok || bin != "/opt/muse" {
		t.Errorf("MUSE_BIN first: bin = %q, available = %v", bin, ok)
	}
}
