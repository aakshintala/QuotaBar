package monitor

import (
	"context"
	"sync"
	"time"
)

const (
	MinimumSnapshotAge = 60 * time.Second
	MinimumInterval    = 60 * time.Second
	FetchTimeout       = 60 * time.Second
)

func ClampInterval(d time.Duration) time.Duration { return max(d, MinimumInterval) }

type RateLimitedError struct{ RetryAt time.Time }

func (e *RateLimitedError) Error() string {
	return "Rate limited. Retrying at " + e.RetryAt.Format(time.RFC3339) + "."
}

type Provider interface {
	ID() string
	Available() bool
	Fetch(ctx context.Context) (Snapshot, error)
}
type Clock interface {
	Now() time.Time
	Sleep(ctx context.Context, d time.Duration) error
}
type SystemClock struct{}

func (SystemClock) Now() time.Time { return time.Now() }
func (SystemClock) Sleep(ctx context.Context, d time.Duration) error {
	timer := time.NewTimer(d)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-timer.C:
		return nil
	}
}

type Monitor struct {
	providers   []Provider
	isEnabled   func(string) bool
	displayName func(string) string
	clock       Clock
	mu          sync.Mutex
	snapshots   map[string]Snapshot
	errs        map[string]error
	inflight    map[string]chan struct{}
}
type Option func(*Monitor)

func WithClock(c Clock) Option                 { return func(m *Monitor) { m.clock = c } }
func WithIsEnabled(f func(string) bool) Option { return func(m *Monitor) { m.isEnabled = f } }
func WithDisplayName(f func(string) string) Option {
	return func(m *Monitor) { m.displayName = f }
}
func New(providers []Provider, opts ...Option) *Monitor {
	m := &Monitor{providers: providers, snapshots: map[string]Snapshot{}, errs: map[string]error{}, inflight: map[string]chan struct{}{}}
	m.isEnabled = func(string) bool { return true }
	m.displayName = func(id string) string { return id }
	m.clock = SystemClock{}
	for _, opt := range opts {
		opt(m)
	}
	return m
}
func (m *Monitor) DisplayName(id string) string { return m.displayName(id) }
func (m *Monitor) Providers() []Provider        { return m.providers }
func (m *Monitor) Enabled(id string) bool       { return m.isEnabled(id) }
func (m *Monitor) SnapshotOf(id string) (Snapshot, bool) {
	m.mu.Lock()
	defer m.mu.Unlock()
	s, ok := m.snapshots[id]
	return s, ok
}
func (m *Monitor) LastError(id string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.errs[id]
}
func (m *Monitor) Refresh(ctx context.Context, force bool) {
	var wg sync.WaitGroup
	for _, p := range m.providers {
		if !m.isEnabled(p.ID()) || (!force && m.fresh(p.ID())) {
			continue
		}
		m.mu.Lock()
		done, ok := m.inflight[p.ID()]
		if !ok {
			done = make(chan struct{})
			m.inflight[p.ID()] = done
		}
		m.mu.Unlock()
		wg.Add(1)
		if !ok {
			// The fetch outlives any one caller: a waiter that gives up must not cancel it for the others.
			fctx, cancel := context.WithTimeout(context.WithoutCancel(ctx), FetchTimeout)
			go func() { defer cancel(); m.fetchAndStore(fctx, p, done) }()
		}
		go func() {
			defer wg.Done()
			select {
			case <-done:
			case <-ctx.Done():
			}
		}()
	}
	wg.Wait()
}
func (m *Monitor) fresh(id string) bool {
	m.mu.Lock()
	defer m.mu.Unlock()
	snap, ok := m.snapshots[id]
	return ok && m.clock.Now().Sub(snap.CapturedAt) < MinimumSnapshotAge
}
func (m *Monitor) fetchAndStore(ctx context.Context, p Provider, done chan struct{}) {
	defer func() {
		m.mu.Lock()
		delete(m.inflight, p.ID())
		m.mu.Unlock()
		close(done)
	}()
	if !p.Available() {
		return
	}
	snap, err := p.Fetch(ctx)
	m.mu.Lock()
	defer m.mu.Unlock()
	if err != nil {
		m.errs[p.ID()] = err
		return
	}
	m.snapshots[p.ID()] = snap
	delete(m.errs, p.ID())
}
func (m *Monitor) StartLoop(ctx context.Context, interval time.Duration) {
	interval = ClampInterval(interval)
	m.Refresh(ctx, false)
	for ctx.Err() == nil {
		if err := m.clock.Sleep(ctx, interval); err != nil {
			return
		}
		m.Refresh(ctx, false)
	}
}
