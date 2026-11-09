// Package muse reads Muse subscription quota by running the Muse CLI's
// own `muse usage --json`. quotad never touches the CLI's Keychain item or
// auth file: that item trusts only the muse binary and is rewritten on every
// token refresh, so any other reader triggers a SecurityAgent password
// dialog. The CLI reads its own login without a prompt.
//
// quotad persists only quota percentages and resets under ~/.quotabar/muse:
// a cooldown stamp, so the CLI runs at most once every five minutes, plus
// the last observation, so a restart can still show the last reading when a
// run fails.
package muse

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"quotad/creds"
	"quotad/monitor"
)

const (
	cooldown         = 5 * time.Minute
	runTimeout       = 20 * time.Second
	maxOutputBytes   = 256 * 1024
	maxStderrExcerpt = 200
	appleEpochOffset = 978307200

	stampName = "cli.request"
	cacheName = "cli.json"
)

// Runner runs bin with args in dir, with no stdin, and returns its stdout
// and stderr. Tests stub it so they never exec the real CLI.
type Runner func(ctx context.Context, dir, bin string, args ...string) (stdout, stderr []byte, err error)

// Option customizes a Provider.
type Option func(*Provider)

// WithNow pins the clock.
func WithNow(now func() time.Time) Option { return func(p *Provider) { p.now = now } }

// WithLookupEnv pins environment lookups so tests never read the real home.
func WithLookupEnv(f func(string) string) Option { return func(p *Provider) { p.lookupEnv = f } }

// WithLookPath pins the PATH search for the muse binary.
func WithLookPath(f func(string) (string, error)) Option { return func(p *Provider) { p.lookPath = f } }

// WithRunner pins the process runner.
func WithRunner(r Runner) Option { return func(p *Provider) { p.run = r } }

// Provider fetches Muse subscription quota. The monitor runs at most one
// Fetch per provider at a time, so mem and lastErr need no lock.
type Provider struct {
	now       func() time.Time
	lookupEnv func(string) string
	lookPath  func(string) (string, error)
	run       Runner
	mem       bool
	lastErr   error // the failure of the last run, until one succeeds
}

// New builds a Muse provider.
func New(opts ...Option) *Provider {
	p := &Provider{now: time.Now,
		lookupEnv: func(k string) string { v, _ := os.LookupEnv(k); return v },
		lookPath:  exec.LookPath,
		run:       runCLI}
	for _, opt := range opts {
		opt(p)
	}
	return p
}

// runCLI is the production Runner. Stdin stays unset (/dev/null).
func runCLI(ctx context.Context, dir, bin string, args ...string) ([]byte, []byte, error) {
	cmd := exec.CommandContext(ctx, bin, args...)
	cmd.Dir = dir
	cmd.WaitDelay = 2 * time.Second // do not hang on pipes a child keeps open
	var stdout, stderr bytes.Buffer
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	err := cmd.Run()
	return stdout.Bytes(), stderr.Bytes(), err
}

// ID returns the provider id (settings key and feed id).
func (p *Provider) ID() string { return "muse" }

// Available is cheap: a muse binary resolves, without running it.
func (p *Provider) Available() bool {
	_, ok := p.binary()
	return ok
}

// binary resolves the CLI: $MUSE_BIN when set, else ~/.local/bin/muse when
// present (launchd gives quotad a minimal PATH), else a PATH search.
func (p *Provider) binary() (string, bool) {
	if bin := p.lookupEnv("MUSE_BIN"); bin != "" {
		return bin, true
	}
	if home := p.home(); home != "" {
		local := filepath.Join(home, ".local/bin/muse")
		if info, err := os.Stat(local); err == nil && !info.IsDir() {
			return local, true
		}
	}
	if bin, err := p.lookPath("muse"); err == nil {
		return bin, true
	}
	return "", false
}

// Fetch returns the current quota snapshot.
func (p *Provider) Fetch(ctx context.Context) (monitor.Snapshot, error) {
	now := p.now()
	dir := p.cacheDir()
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return monitor.Snapshot{}, fmt.Errorf("muse: quota cache unavailable: %w", err)
	}
	// The cooldown is checked before anything runs. It is QuotaBar's own
	// bound, not a server rate limit, so repeat the last outcome.
	stamp := filepath.Join(dir, stampName)
	if ts, found := readStamp(stamp); found && now.Before(ts.Add(cooldown)) {
		if p.lastErr != nil {
			return monitor.Snapshot{}, p.lastErr
		}
		if prev, ok := p.loadPersisted(now); ok {
			return prev, nil
		}
		return monitor.Snapshot{}, fmt.Errorf("Muse quota refreshes at most every %v; next refresh at %s.",
			cooldown, ts.Add(cooldown).Format(time.RFC3339))
	}
	bin, ok := p.binary()
	if !ok {
		return monitor.Snapshot{}, errors.New("Muse CLI not found. Install it in ~/.local/bin or set MUSE_BIN.")
	}
	// Reserve the cooldown before the run, so a failure also consumes it.
	if err := creds.WriteFileAtomic(stamp, []byte(strconv.FormatInt(now.Unix(), 10))); err != nil {
		return monitor.Snapshot{}, fmt.Errorf("muse: quota cache unavailable: %w", err)
	}
	snap, hint, err := p.usage(ctx, bin, now)
	if err != nil {
		if prev, ok := p.loadPersisted(now); ok && !p.mem {
			// First fetch after a restart: no in-memory snapshot exists
			// yet, so serve the unexpired persisted observation and log
			// the failure instead of failing the refresh.
			slog.Warn("muse: quota fetch failed, serving persisted observation", "err", err)
			p.lastErr = nil
			return prev, nil
		}
		p.lastErr = err
		return monitor.Snapshot{}, err
	}
	// A quota-less response must not wipe a previous observation: the
	// monitor keeps its snapshot when Fetch errors.
	if len(snap.Quotas) == 0 {
		if p.mem || p.hasPersisted(now) {
			p.lastErr = errors.New("Muse has not returned current quota data; showing the last known reading.")
		} else {
			p.lastErr = fmt.Errorf("Muse has no quota data yet: %s", hint)
		}
		return monitor.Snapshot{}, p.lastErr
	}
	if err := p.savePersisted(snap); err != nil {
		slog.Warn("muse: quota observation not persisted", "err", err)
	}
	p.mem, p.lastErr = true, nil
	return snap, nil
}

// usage runs `muse usage --json` and parses it. hint explains a reply
// without quota data.
func (p *Provider) usage(ctx context.Context, bin string, now time.Time) (monitor.Snapshot, string, error) {
	rctx, cancel := context.WithTimeout(ctx, runTimeout)
	defer cancel()
	stdout, stderr, err := p.run(rctx, p.home(), bin, "usage", "--json")
	if err != nil {
		if errors.Is(rctx.Err(), context.DeadlineExceeded) {
			return monitor.Snapshot{}, "", fmt.Errorf("`muse usage` timed out after %v", runTimeout)
		}
		msg := fmt.Sprintf("`muse usage` failed: %v", err)
		if excerpt := stderrExcerpt(stderr); excerpt != "" {
			msg += ": " + excerpt
		}
		return monitor.Snapshot{}, "", errors.New(msg)
	}
	if len(stdout) > maxOutputBytes {
		return monitor.Snapshot{}, "", errors.New("`muse usage` output exceeded the size limit")
	}
	return parseUsage(stdout, now)
}

// stderrExcerpt trims stderr to one short excerpt for error messages.
func stderrExcerpt(stderr []byte) string {
	s := strings.Join(strings.Fields(string(stderr)), " ")
	if r := []rune(s); len(r) > maxStderrExcerpt {
		s = string(r[:maxStderrExcerpt]) + "…"
	}
	return s
}

// usageWindow is one quota window of `muse usage --json`.
type usageWindow struct {
	UsedPercent *float64 `json:"usedPercent"`
	ResetsAtMs  *float64 `json:"resetsAtMs"`
}

// parseUsage decodes `muse usage --json` (schemaVersion 1). Missing windows
// stay unknown (no invented 100%), and windows whose reset already passed
// are omitted. A null allowance yields an empty snapshot and its hint.
func parseUsage(data []byte, now time.Time) (monitor.Snapshot, string, error) {
	var resp struct {
		SchemaVersion *int `json:"schemaVersion"`
		Allowance     *struct {
			Tier   *string      `json:"tier"`
			Window *usageWindow `json:"window"`
			Weekly *usageWindow `json:"weekly"`
		} `json:"allowance"`
		Reason *string `json:"reason"`
		Hint   *string `json:"hint"`
	}
	if err := json.Unmarshal(data, &resp); err != nil || resp.SchemaVersion == nil {
		return monitor.Snapshot{}, "", errors.New("`muse usage --json` output could not be decoded")
	}
	if *resp.SchemaVersion != 1 {
		return monitor.Snapshot{}, "", fmt.Errorf("unsupported muse usage schema version %d; update quotad", *resp.SchemaVersion)
	}
	snap := monitor.Snapshot{ProviderID: "muse", CapturedAt: now}
	if resp.Allowance == nil {
		hint := "the Muse CLI returned no usage"
		if resp.Hint != nil && *resp.Hint != "" {
			hint = *resp.Hint
		} else if resp.Reason != nil && *resp.Reason != "" {
			hint = *resp.Reason
		}
		return snap, hint, nil
	}
	if resp.Allowance.Tier != nil {
		tier := monitor.CustomTier(*resp.Allowance.Tier)
		snap.Tier = &tier
	}
	for _, win := range []struct {
		w   *usageWindow
		typ monitor.QuotaType
	}{{resp.Allowance.Window, monitor.SessionType()}, {resp.Allowance.Weekly, monitor.WeeklyType()}} {
		if win.w == nil {
			continue
		}
		var reset *time.Time
		if win.w.ResetsAtMs != nil {
			t := time.UnixMilli(int64(*win.w.ResetsAtMs)).UTC()
			reset = &t
		}
		if q, ok := windowQuota(win.w.UsedPercent, reset, win.typ, now); ok {
			snap.Quotas = append(snap.Quotas, q)
		}
	}
	return snap, "the Muse CLI returned no current quota windows", nil
}

// hasPersisted reports whether an unexpired observation is on disk.
func (p *Provider) hasPersisted(now time.Time) bool {
	_, ok := p.loadPersisted(now)
	return ok
}

// cacheDir is ~/.quotabar/muse.
func (p *Provider) cacheDir() string { return filepath.Join(p.home(), ".quotabar/muse") }

// home returns $HOME; tests always inject it, and the daemon runs with it set.
func (p *Provider) home() string { return p.lookupEnv("HOME") }

// readStamp parses a persisted cooldown stamp (Unix seconds, possibly
// fractional).
func readStamp(path string) (time.Time, bool) {
	data, err := creds.ReadFile(path)
	if err != nil {
		return time.Time{}, false
	}
	secs, err := strconv.ParseFloat(strings.TrimSpace(string(data)), 64)
	if err != nil {
		return time.Time{}, false
	}
	return unixFloat(secs), true
}

// unixFloat converts float Unix seconds to time, keeping the fractional part.
func unixFloat(secs float64) time.Time {
	whole := int64(secs)
	return time.Unix(whole, int64((secs-float64(whole))*1e9)).UTC()
}

// windowQuota converts one used-percent window to a quota, or false when the
// window is absent or its reset already passed.
func windowQuota(used *float64, reset *time.Time, typ monitor.QuotaType, now time.Time) (monitor.Quota, bool) {
	if used == nil || (reset != nil && !reset.After(now)) {
		return monitor.Quota{}, false
	}
	q := monitor.NewPercentQuota(100-min(100, max(0, *used)), typ, "muse")
	q.ResetsAt = reset
	return q, true
}

// cachedWindow is one persisted quota window (Swift-compatible JSON).
type cachedWindow struct {
	Type      string   `json:"type"`
	Remaining float64  `json:"remaining"`
	ResetsAt  *float64 `json:"resetsAt"`
}

// cachedSnapshot is the persisted observation (Swift-compatible JSON with
// Apple reference-date doubles).
type cachedSnapshot struct {
	ProviderID string         `json:"providerId"`
	CapturedAt float64        `json:"capturedAt"`
	Tier       *string        `json:"tier"`
	Windows    []cachedWindow `json:"windows"`
}

// appleToTime converts Apple reference-date seconds to time.
func appleToTime(secs float64) time.Time {
	return unixFloat(secs + appleEpochOffset)
}

// timeToApple converts time to Apple reference-date seconds.
func timeToApple(t time.Time) float64 {
	return float64(t.Unix()-appleEpochOffset) + float64(t.Nanosecond())/1e9
}

// loadPersisted returns the persisted observation with expired windows
// filtered out, or false when none unexpired remains.
func (p *Provider) loadPersisted(now time.Time) (monitor.Snapshot, bool) {
	data, err := creds.ReadFile(filepath.Join(p.cacheDir(), cacheName))
	if err != nil {
		return monitor.Snapshot{}, false
	}
	var cached cachedSnapshot
	if err := json.Unmarshal(data, &cached); err != nil || cached.ProviderID != "muse" {
		return monitor.Snapshot{}, false
	}
	snap := monitor.Snapshot{ProviderID: "muse", CapturedAt: appleToTime(cached.CapturedAt)}
	if cached.Tier != nil {
		tier := monitor.CustomTier(*cached.Tier)
		snap.Tier = &tier
	}
	for _, w := range cached.Windows {
		typ := monitor.WeeklyType()
		if w.Type == "session" {
			typ = monitor.SessionType()
		}
		var reset *time.Time
		if w.ResetsAt != nil {
			t := appleToTime(*w.ResetsAt)
			reset = &t
		}
		used := 100 - w.Remaining // the cache stores remaining, windowQuota takes used
		if q, ok := windowQuota(&used, reset, typ, now); ok {
			snap.Quotas = append(snap.Quotas, q)
		}
	}
	return snap, len(snap.Quotas) > 0
}

// savePersisted stores quota percentages and resets only: no tokens, keys,
// emails or other account data ever reach the cache directory.
func (p *Provider) savePersisted(snap monitor.Snapshot) error {
	cached := cachedSnapshot{ProviderID: "muse", CapturedAt: timeToApple(snap.CapturedAt)}
	if snap.Tier != nil {
		cached.Tier = &snap.Tier.Badge
	}
	for _, q := range snap.Quotas {
		if q.PercentRemaining == nil {
			continue
		}
		typ := "weekly"
		if q.Type.Kind == monitor.KindSession {
			typ = "session"
		}
		w := cachedWindow{Type: typ, Remaining: *q.PercentRemaining}
		if q.ResetsAt != nil {
			secs := timeToApple(*q.ResetsAt)
			w.ResetsAt = &secs
		}
		cached.Windows = append(cached.Windows, w)
	}
	// Marshal cannot fail: the struct holds only strings and floats.
	data, _ := json.Marshal(cached)
	return creds.WriteFileAtomic(filepath.Join(p.cacheDir(), cacheName), data)
}
