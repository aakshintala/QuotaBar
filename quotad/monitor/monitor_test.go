package monitor

import (
	"context"
	"errors"
	"runtime"
	"sync"
	"testing"
	"time"
)

func TestAbsoluteStatusThresholds(t *testing.T) {
	cases := []struct {
		remaining float64
		want      Status
	}{
		{100, Healthy}, {75, Healthy}, {51, Healthy}, {50, Healthy},
		{49, Warning}, {35, Warning}, {20, Warning},
		{19, Critical}, {10, Critical}, {1, Critical},
		{0, Depleted}, {-1, Depleted}, {-100, Depleted},
	}
	for _, c := range cases {
		if got := FromPercent(c.remaining); got != c.want {
			t.Errorf("FromPercent(%v) = %v, want %v", c.remaining, got, c.want)
		}
	}
}

func TestPaceAwareStatus(t *testing.T) {
	f := func(v float64) *float64 { return &v }
	cases := []struct {
		name      string
		remaining float64
		elapsed   *float64
		want      Status
	}{
		{"slow burn is healthy", 43, f(85), Healthy},
		{"fast burn warns", 47, f(8.5), Warning},
		{"depleted is always depleted", 0, f(99), Depleted},
		{"low but lasting is healthy", 15, f(90), Healthy},
		{"low and burning is critical", 15, f(10), Critical},
		{"exhausts before reset is critical", 1.8, f(87.8), Critical},
		{"high remaining ignores burn", 90, f(5), Healthy},
		{"zero elapsed falls back to absolute", 43, f(0), Warning},
		{"nil elapsed falls back to absolute", 35, nil, Warning},
		{"burn matters under 50", 45, f(30), Warning},
	}
	for _, c := range cases {
		if got := FromPace(c.remaining, c.elapsed); got != c.want {
			t.Errorf("%s: FromPace(%v) = %v, want %v", c.name, c.remaining, got, c.want)
		}
	}
}

func TestPercentQuotaCapsAt100(t *testing.T) {
	q := NewPercentQuota(150, SessionType(), "claude")
	if *q.PercentRemaining != 100 {
		t.Errorf("capped = %v, want 100", *q.PercentRemaining)
	}
	q = NewPercentQuota(-5, SessionType(), "claude")
	if *q.PercentRemaining != -5 {
		t.Errorf("negative should pass through, got %v", *q.PercentRemaining)
	}
}

func TestCompactResetTime(t *testing.T) {
	now := time.Now()
	cases := []struct {
		in   time.Duration
		want string
	}{
		{2*24*time.Hour + 5*time.Hour, "2d"},
		{3*time.Hour + 15*time.Minute, "3h"},
		{45 * time.Minute, "45m"},
		{30 * time.Second, "soon"},
	}
	for _, c := range cases {
		at := now.Add(c.in)
		q := Quota{PercentRemaining: f64(35), Type: WeeklyType(), ResetsAt: &at}
		if got := q.CompactResetAt(now); got != c.want {
			t.Errorf("CompactResetAt(%v) = %q, want %q", c.in, got, c.want)
		}
	}
	q := Quota{PercentRemaining: f64(35), Type: WeeklyType()}
	if got := q.CompactResetAt(now); got != "" {
		t.Errorf("no resetsAt should be empty, got %q", got)
	}
}

func f64(v float64) *float64 { return &v }

func TestBalanceFormatting(t *testing.T) {
	if got := USD.Format(50); got != "$50.00" {
		t.Errorf("usd 50 = %q", got)
	}
	if got := USD.Format(1234.5); got != "$1,234.50" {
		t.Errorf("usd 1234.5 = %q", got)
	}
	if got := Credits.Format(1234); got != "1,234 credits" {
		t.Errorf("credits 1234 = %q", got)
	}
	if got := Credits.Format(12.5); got != "12.5 credits" {
		t.Errorf("credits 12.5 = %q", got)
	}
}

func TestUSDFormatRoundsToCents(t *testing.T) {
	for _, c := range []struct {
		amount float64
		want   string
	}{
		{1.15, "$1.15"},
		{19.99, "$19.99"},
		{50, "$50.00"},
		{1234.5, "$1,234.50"},
		{0, "$0.00"},
	} {
		if got := USD.Format(c.amount); got != c.want {
			t.Errorf("usd %v = %q, want %q", c.amount, got, c.want)
		}
	}
}

func TestBalanceOnlyIsAlwaysHealthy(t *testing.T) {
	left := 750.0
	unit := Credits
	q := Quota{Type: TimeLimitType("Credits"), BalanceRemaining: &left, BalanceUnit: &unit}
	if !q.IsBalanceOnly() {
		t.Error("should be balance-only")
	}
	if q.StatusAt(time.Now()) != Healthy {
		t.Error("balance-only should always be healthy")
	}
	snap := Snapshot{ProviderID: "codex", Quotas: []Quota{q}, CapturedAt: time.Now()}
	if snap.OverallStatusAt(time.Now()) != Healthy {
		t.Error("overall status should ignore balance-only meters")
	}
}

func TestQuotaTypeKeys(t *testing.T) {
	cases := []struct {
		typ   QuotaType
		key   string
		label string
	}{
		{SessionType(), "session", "Session"},
		{WeeklyType(), "weekly", "Weekly"},
		{ModelType("fable"), "model:fable", "Fable"},
		{TimeLimitType("Monthly"), "time:Monthly", "Monthly"},
	}
	for _, c := range cases {
		if c.typ.Key() != c.key || c.typ.DisplayName() != c.label {
			t.Errorf("got (%q, %q), want (%q, %q)", c.typ.Key(), c.typ.DisplayName(), c.key, c.label)
		}
	}
	if got := TimeLimitType("Monthly").Duration(); got != 30*24*time.Hour {
		t.Errorf("monthly duration = %v", got)
	}
	if got := SessionType().Duration(); got != 5*time.Hour {
		t.Errorf("session duration = %v", got)
	}
}

// fakeProvider is a scriptable Provider.
type fakeProvider struct {
	id        string
	enabled   bool
	available bool
	mu        sync.Mutex
	calls     int
	release   chan struct{}
	captured  time.Time
	snapshot  Snapshot
	err       error
}

func (f *fakeProvider) ID() string      { return f.id }
func (f *fakeProvider) Available() bool { return f.available }
func (f *fakeProvider) Fetch(ctx context.Context) (Snapshot, error) {
	f.mu.Lock()
	f.calls++
	f.mu.Unlock()
	if f.release != nil {
		select {
		case <-f.release:
		case <-ctx.Done():
			return Snapshot{}, ctx.Err()
		}
	}
	if f.err != nil {
		return Snapshot{}, f.err
	}
	if f.snapshot.ProviderID != "" {
		return f.snapshot, nil
	}
	return Snapshot{ProviderID: f.id, Quotas: []Quota{
		NewPercentQuota(65, SessionType(), f.id),
	}, CapturedAt: f.captured}, nil
}

func (f *fakeProvider) numCalls() int {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.calls
}

type manualClock struct {
	mu     sync.Mutex
	now    time.Time
	sleeps []time.Duration
}

func (c *manualClock) Now() time.Time {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.now
}

func (c *manualClock) Sleep(ctx context.Context, d time.Duration) error {
	c.mu.Lock()
	c.sleeps = append(c.sleeps, d)
	c.mu.Unlock()
	<-ctx.Done()
	return ctx.Err()
}

func TestRefreshStoresSnapshot(t *testing.T) {
	p := &fakeProvider{id: "claude", enabled: true, available: true}
	m := New([]Provider{p}, WithClock(&manualClock{now: time.Now()}))
	m.Refresh(context.Background(), false)
	snap, ok := m.SnapshotOf("claude")
	if !ok || len(snap.Quotas) != 1 {
		t.Fatalf("snapshot = %+v, ok = %v", snap, ok)
	}
	if m.LastError("claude") != nil {
		t.Error("no error expected")
	}
}

func TestRefreshSkipsUnavailable(t *testing.T) {
	p := &fakeProvider{id: "claude", available: true} // enabled irrelevant; Available false
	p.available = false
	m := New([]Provider{p})
	m.Refresh(context.Background(), false)
	if _, ok := m.SnapshotOf("claude"); ok {
		t.Error("unavailable provider should leave no snapshot")
	}
}

func TestRefreshSkipsDisabled(t *testing.T) {
	p := &fakeProvider{id: "claude", available: true}
	m := New([]Provider{p}, WithIsEnabled(func(string) bool { return false }))
	m.Refresh(context.Background(), false)
	if _, ok := m.SnapshotOf("claude"); ok {
		t.Error("disabled provider should not be refreshed")
	}
	if p.numCalls() != 0 {
		t.Error("disabled provider should not be probed")
	}
}

func TestFreshSnapshotSkippedUnlessForced(t *testing.T) {
	now := time.Now()
	p := &fakeProvider{id: "claude", available: true, captured: now}
	m := New([]Provider{p}, WithClock(&manualClock{now: now}))
	m.Refresh(context.Background(), false)
	m.Refresh(context.Background(), false)
	if p.numCalls() != 1 {
		t.Errorf("fresh snapshot should be kept, calls = %d", p.numCalls())
	}
	m.Refresh(context.Background(), true)
	if p.numCalls() != 2 {
		t.Errorf("forced refresh should re-probe, calls = %d", p.numCalls())
	}
}

func TestStaleSnapshotReprobed(t *testing.T) {
	now := time.Now()
	p := &fakeProvider{id: "claude", available: true, captured: now.Add(-120 * time.Second)}
	m := New([]Provider{p}, WithClock(&manualClock{now: now}))
	m.Refresh(context.Background(), false)
	m.Refresh(context.Background(), false)
	if p.numCalls() != 2 {
		t.Errorf("stale snapshot should re-probe, calls = %d", p.numCalls())
	}
}

func TestConcurrentRefreshProbesOnce(t *testing.T) {
	p := &fakeProvider{id: "claude", available: true, release: make(chan struct{})}
	m := New([]Provider{p})
	done := make(chan struct{})
	go func() { m.Refresh(context.Background(), false); close(done) }()
	go m.Refresh(context.Background(), true)
	time.Sleep(50 * time.Millisecond)
	close(p.release)
	<-done
	// The forced join waits for the same in-flight fetch.
	time.Sleep(50 * time.Millisecond)
	if p.numCalls() != 1 {
		t.Errorf("joined refresh should probe once, calls = %d", p.numCalls())
	}
}

func TestFailureKeepsLastGoodSnapshot(t *testing.T) {
	p := &fakeProvider{id: "claude", available: true}
	m := New([]Provider{p})
	m.Refresh(context.Background(), false)
	before, _ := m.SnapshotOf("claude")
	p.err = errors.New("boom")
	// Force past the freshness rule so the failure path runs.
	m.Refresh(context.Background(), true)
	after, ok := m.SnapshotOf("claude")
	if !ok {
		t.Fatal("failed refresh must keep the last good snapshot")
	}
	if len(after.Quotas) != len(before.Quotas) {
		t.Error("snapshot should be unchanged after failure")
	}
	if m.LastError("claude") == nil {
		t.Error("last error should be recorded")
	}
	// Recovery clears the error.
	p.err = nil
	m.Refresh(context.Background(), true)
	if m.LastError("claude") != nil {
		t.Error("error should clear after a good refresh")
	}
}

func TestOneFailureDoesNotAffectOthers(t *testing.T) {
	good := &fakeProvider{id: "claude", available: true}
	bad := &fakeProvider{id: "codex", available: true, err: errors.New("timeout")}
	m := New([]Provider{good, bad})
	m.Refresh(context.Background(), true)
	if _, ok := m.SnapshotOf("claude"); !ok {
		t.Error("good provider should have a snapshot")
	}
	if _, ok := m.SnapshotOf("codex"); ok {
		t.Error("failed provider should have no snapshot")
	}
}

func TestClampIntervalFloor(t *testing.T) {
	for _, c := range []struct {
		in, want time.Duration
	}{
		{5 * time.Second, 60 * time.Second},
		{0, 60 * time.Second},
		{60 * time.Second, 60 * time.Second},
		{300 * time.Second, 300 * time.Second},
	} {
		if got := ClampInterval(c.in); got != c.want {
			t.Errorf("ClampInterval(%v) = %v, want %v", c.in, got, c.want)
		}
	}
}

func TestBackgroundLoopRefreshesThenSleeps(t *testing.T) {
	p := &fakeProvider{id: "claude", available: true}
	clock := &manualClock{now: time.Now()}
	m := New([]Provider{p}, WithClock(clock))
	ctx, cancel := context.WithCancel(context.Background())
	go func() {
		time.Sleep(100 * time.Millisecond)
		cancel()
	}()
	m.StartLoop(ctx, 30*time.Second) // below the floor
	if p.numCalls() != 1 {
		t.Errorf("loop should refresh once before sleeping, calls = %d", p.numCalls())
	}
	if len(clock.sleeps) != 1 || clock.sleeps[0] != 60*time.Second {
		t.Errorf("loop should sleep the floored interval, sleeps = %v", clock.sleeps)
	}
}

func TestRefreshSpawnsNoWaiterGoroutines(t *testing.T) {
	const blocked = 8
	release := make(chan struct{})
	providers := make([]Provider, blocked)
	for i := range providers {
		providers[i] = &fakeProvider{id: string(rune('a' + i)), available: true, release: release}
	}
	m := New(providers)
	before := runtime.NumGoroutine()
	returned := make(chan struct{})
	go func() { m.Refresh(context.Background(), false); close(returned) }()
	deadline := time.Now().Add(5 * time.Second)
	for {
		total := 0
		for _, p := range providers {
			total += p.(*fakeProvider).numCalls()
		}
		if total == blocked {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("fetches did not start")
		}
		runtime.Gosched()
	}
	time.Sleep(100 * time.Millisecond) // let any waiter goroutines appear
	lowest := runtime.NumGoroutine()
	for i := 0; i < 5; i++ {
		time.Sleep(10 * time.Millisecond)
		if n := runtime.NumGoroutine(); n < lowest {
			lowest = n
		}
	}
	close(release)
	select {
	case <-returned:
	case <-time.After(5 * time.Second):
		t.Fatal("refresh did not return")
	}
	// Only the fetches may add goroutines: 1 runner + blocked fetches.
	if growth := lowest - before; growth > blocked+3 {
		t.Errorf("goroutines grew by %d, want at most %d: Refresh must not spawn waiter goroutines", growth, blocked+3)
	}
}

func TestStaleCheckAndInflightRegisterAreAtomic(t *testing.T) {
	for round := 0; round < 20; round++ {
		if !atomicRound() {
			t.Fatalf("round %d: a caller that saw a stale snapshot started a second fetch", round)
		}
	}
}

func atomicRound() bool {
	now := time.Now()
	p := &fakeProvider{id: "claude", available: true, release: make(chan struct{}), captured: now}
	m := New([]Provider{p}, WithClock(&manualClock{now: now}))
	m.snapshots["claude"] = Snapshot{ProviderID: "claude", CapturedAt: now.Add(-time.Hour)}
	first := make(chan struct{})
	go func() { m.Refresh(context.Background(), false); close(first) }()
	deadline := time.Now().Add(5 * time.Second)
	for p.numCalls() == 0 {
		if time.Now().After(deadline) {
			return false
		}
		runtime.Gosched()
	}
	// Churn the monitor mutex: this stretches both the fetch's
	// store-then-delete gap and each racer's check-then-register gap,
	// so a racer that checked while stale registers after the delete.
	stop := make(chan struct{})
	defer close(stop)
	for s := 0; s < 8; s++ {
		go func() {
			for {
				select {
				case <-stop:
					return
				default:
					m.SnapshotOf("claude")
				}
			}
		}()
	}
	const racers = 32
	var wg sync.WaitGroup
	wg.Add(racers + 1)
	go func() { defer wg.Done(); <-first }()
	close(p.release)
	for k := 0; k < racers; k++ {
		go func() { defer wg.Done(); m.Refresh(context.Background(), false) }()
	}
	wg.Wait()
	return p.numCalls() == 1
}

func TestCallerGivingUpDoesNotCancelSharedFetch(t *testing.T) {
	p := &fakeProvider{id: "claude", available: true, release: make(chan struct{}), captured: time.Now()}
	m := New([]Provider{p})
	ctx, cancel := context.WithCancel(context.Background())
	returned := make(chan struct{})
	go func() { m.Refresh(ctx, true); close(returned) }()
	for p.numCalls() == 0 {
		time.Sleep(time.Millisecond)
	}
	cancel()
	<-returned
	close(p.release)
	m.Refresh(context.Background(), false) // joins the still-running fetch
	if _, ok := m.SnapshotOf("claude"); !ok {
		t.Fatalf("fetch was cancelled with its first caller; err=%v", m.LastError("claude"))
	}
}
