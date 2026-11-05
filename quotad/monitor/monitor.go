// Package monitor is the single source of truth for quota state: it
// refreshes providers concurrently, caches snapshots, derives pace-aware
// status, and runs the background loop.
package monitor

import (
	"context"
	"sync"
	"time"
)

const (
	// MinimumSnapshotAge is how long a cached snapshot stays reusable.
	MinimumSnapshotAge = 60 * time.Second
	// MinimumInterval floors the background loop interval.
	MinimumInterval = 60 * time.Second
	// FetchTimeout bounds a single provider fetch.
	FetchTimeout = 60 * time.Second
)

// ClampInterval floors d at MinimumInterval.
func ClampInterval(d time.Duration) time.Duration { return max(d, MinimumInterval) }

// RateLimitedError marks a provider as throttled until RetryAt.
type RateLimitedError struct{ RetryAt time.Time }

// Error describes when the provider stops being throttled.
func (e *RateLimitedError) Error() string {
	return "Rate limited. Retrying at " + e.RetryAt.Format(time.RFC3339) + "."
}

// Provider is one quota source: an id, an availability gate, and a fetch.
type Provider interface {
	ID() string
	Available() bool
	Fetch(ctx context.Context) (Snapshot, error)
}

// Clock abstracts time so the background loop is testable.
type Clock interface {
	Now() time.Time
	Sleep(ctx context.Context, d time.Duration) error
}

// SystemClock is the wall-clock Clock.
type SystemClock struct{}

// Now returns the current wall-clock time.
func (SystemClock) Now() time.Time { return time.Now() }

// Sleep waits d or until ctx ends.
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

// Monitor holds the cached snapshots and errors and refreshes providers.
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

// Option customizes a Monitor.
type Option func(*Monitor)

// WithClock sets the clock (tests use a manual one).
func WithClock(c Clock) Option { return func(m *Monitor) { m.clock = c } }

// WithIsEnabled sets the per-provider enable check.
func WithIsEnabled(f func(string) bool) Option { return func(m *Monitor) { m.isEnabled = f } }

// WithDisplayName sets the human name shown for a provider id.
func WithDisplayName(f func(string) string) Option {
	return func(m *Monitor) { m.displayName = f }
}

// New builds a Monitor over providers, enabled by default.
func New(providers []Provider, opts ...Option) *Monitor {
	m := &Monitor{
		providers: providers,
		snapshots: map[string]Snapshot{},
		errs:      map[string]error{},
		inflight:  map[string]chan struct{}{},
	}
	m.isEnabled = func(string) bool { return true }
	m.displayName = func(id string) string { return id }
	m.clock = SystemClock{}

	for _, opt := range opts {
		opt(m)
	}

	return m
}

// DisplayName returns the human name for a provider id.
func (m *Monitor) DisplayName(id string) string { return m.displayName(id) }

// Providers returns the registered providers.
func (m *Monitor) Providers() []Provider { return m.providers }

// Enabled reports whether the provider is switched on.
func (m *Monitor) Enabled(id string) bool { return m.isEnabled(id) }

// SnapshotOf returns the cached snapshot for id, if any.
func (m *Monitor) SnapshotOf(id string) (Snapshot, bool) {
	m.mu.Lock()
	defer m.mu.Unlock()

	s, ok := m.snapshots[id]
	return s, ok
}

// LastError returns the last fetch error for id, if any.
func (m *Monitor) LastError(id string) error {
	m.mu.Lock()
	defer m.mu.Unlock()

	return m.errs[id]
}

// Refresh refreshes every enabled provider whose snapshot is older than
// MinimumSnapshotAge, unless force is set. A caller that arrives while a
// fetch is in flight joins it instead of starting its own.
func (m *Monitor) Refresh(ctx context.Context, force bool) {
	var waiting []chan struct{}

	for _, p := range m.providers {
		if !m.isEnabled(p.ID()) {
			continue
		}

		// The freshness check and the in-flight registration share one
		// lock acquisition, so a caller that saw a stale snapshot cannot
		// start a second fetch after another caller has just finished one.
		m.mu.Lock()
		snap, ok := m.snapshots[p.ID()]
		fresh := ok && m.clock.Now().Sub(snap.CapturedAt) < MinimumSnapshotAge
		done, inflight := m.inflight[p.ID()]
		if !force && fresh {
			m.mu.Unlock()
			continue
		}
		if !inflight {
			done = make(chan struct{})
			m.inflight[p.ID()] = done
		}
		m.mu.Unlock()

		if !inflight {
			// The fetch outlives any one caller: a waiter that gives up
			// must not cancel it for the others. Only the fetch itself
			// runs in a goroutine; each caller waits inline below.
			fctx, cancel := context.WithTimeout(context.WithoutCancel(ctx), FetchTimeout)
			go func() {
				defer cancel()
				m.fetchAndStore(fctx, p, done)
			}()
		}
		waiting = append(waiting, done)
	}

	for _, done := range waiting {
		select {
		case <-done:
		case <-ctx.Done():
			return
		}
	}
}

// fetchAndStore runs one fetch and records its snapshot or error, then
// wakes every caller waiting on done.
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

// StartLoop refreshes once, then re-refreshes every interval until ctx ends.
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
