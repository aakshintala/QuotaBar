package feed

import (
	"context"
	"errors"
	"time"

	"quotad/monitor"
)

var errBoom = errors.New("boom")

// staticProvider serves a fixed snapshot or error and counts fetches.
type staticProvider struct {
	id       string
	snapshot monitor.Snapshot
	err      error
	fetches  int
}

func (p *staticProvider) ID() string      { return p.id }
func (p *staticProvider) Available() bool { return true }
func (p *staticProvider) Fetch(context.Context) (monitor.Snapshot, error) {
	p.fetches++
	if p.err != nil {
		return monitor.Snapshot{}, p.err
	}
	return p.snapshot, nil
}

type fixedClock struct{ now time.Time }

func (c fixedClock) Now() time.Time { return c.now }
func (c fixedClock) Sleep(ctx context.Context, d time.Duration) error {
	<-ctx.Done()
	return ctx.Err()
}
