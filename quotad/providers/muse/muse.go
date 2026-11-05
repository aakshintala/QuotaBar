// Package muse refreshes Muse subscription quota from Meta's fixed API
// origin using the Muse CLI's existing OAuth login. It never refreshes or
// writes credentials, never calls the key-issuance endpoint, and persists
// only quota percentages and resets (never tokens or account data) under
// ~/.quotabar/muse: a per-login cooldown stamp plus the last observation,
// so a restart can still show the last reading when a fetch fails.
package muse

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"time"

	"quotad/creds"
	"quotad/monitor"
)

const (
	cooldown           = 5 * time.Minute
	requestTimeout     = 15 * time.Second
	maxBodyBytes       = 256 * 1024
	maxCredentialBytes = 1 << 20
	appleEpochOffset   = 978307200
)

// errRedirectRefused aborts any redirect of the token-bearing request.
var errRedirectRefused = errors.New("muse: redirects are refused")

// Keychain failure sentinels, aliased to creds so callers map the same
// values production KeychainRead returns from the security exit statuses.
var (
	// ErrKeychainNotFound means the CLI's Keychain item does not exist.
	ErrKeychainNotFound = creds.ErrKeychainNotFound
	// ErrKeychainDenied means the Keychain read would prompt or was denied.
	ErrKeychainDenied = creds.ErrKeychainDenied
)

// Option customizes a Provider.
type Option func(*Provider)

// WithBaseURL pins the API origin (tests point it at httptest.Server).
func WithBaseURL(url string) Option { return func(p *Provider) { p.baseURL = url } }

// WithNow pins the clock.
func WithNow(now func() time.Time) Option { return func(p *Provider) { p.now = now } }

// WithLookupEnv pins environment lookups so tests never read the real home.
func WithLookupEnv(f func(string) string) Option { return func(p *Provider) { p.lookupEnv = f } }

// WithKeychainRead pins the Keychain read (tests stub it; production uses
// creds.KeychainRead, which never prompts and keeps secrets off argv).
func WithKeychainRead(f func(service, account string) ([]byte, error)) Option {
	return func(p *Provider) { p.keychainRead = f }
}

// WithGOOS pins the OS name used for the Keychain fallback.
func WithGOOS(goos string) Option { return func(p *Provider) { p.goos = goos } }

// Provider fetches Muse subscription quota. The monitor runs at most one
// Fetch per provider at a time, so mem needs no lock.
type Provider struct {
	baseURL      string
	goos         string
	now          func() time.Time
	lookupEnv    func(string) string
	keychainRead func(string, string) ([]byte, error)
	mem          bool
}

// New builds a Muse provider.
func New(opts ...Option) *Provider {
	p := &Provider{baseURL: "https://api.meta.ai", now: time.Now, goos: runtime.GOOS,
		lookupEnv:    func(k string) string { v, _ := os.LookupEnv(k); return v },
		keychainRead: creds.KeychainRead}
	for _, opt := range opts {
		opt(p)
	}
	return p
}

// ID returns the provider id (settings key and feed id).
func (p *Provider) ID() string { return "muse" }

// Available is cheap: the auth file exists, or (on darwin) the Keychain may
// hold the login, which cannot be checked without a process call.
func (p *Provider) Available() bool {
	if _, err := os.Stat(p.authPath()); err == nil {
		return true
	}
	return p.goos == "darwin"
}

// Fetch returns the current quota snapshot.
func (p *Provider) Fetch(ctx context.Context) (monitor.Snapshot, error) {
	now := p.now()
	token, err := p.loadCredentials()
	if err != nil {
		return monitor.Snapshot{}, err
	}
	fp := tokenFingerprint(token)
	dir := p.cacheDir()
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return monitor.Snapshot{}, fmt.Errorf("muse: quota cache unavailable: %w", err)
	}
	// The effective hold is the later of the five-minute cooldown and a
	// persisted 429 retry time, so a short cooldown cannot cut a longer
	// server backoff short after a restart.
	if retryAt, ok := p.nextRetry(fp, now); ok {
		return monitor.Snapshot{}, &monitor.RateLimitedError{RetryAt: retryAt}
	}
	// Reserve the cooldown before the request, so a failure also consumes
	// it and a concurrent Swift app sees the same bound.
	stamp := filepath.Join(dir, fp+".request")
	if err := creds.WriteFileAtomic(stamp, []byte(strconv.FormatInt(now.Unix(), 10))); err != nil {
		return monitor.Snapshot{}, fmt.Errorf("muse: quota cache unavailable: %w", err)
	}
	snap, err := p.request(ctx, token, now)
	if err != nil {
		var rl *monitor.RateLimitedError
		if errors.As(err, &rl) {
			// Persist the server's retry time so a later fetch past the
			// cooldown still waits for it.
			retry := filepath.Join(dir, fp+".retry")
			if werr := creds.WriteFileAtomic(retry, []byte(strconv.FormatInt(rl.RetryAt.UnixNano(), 10))); werr != nil {
				slog.Warn("muse: quota retry not persisted", "err", werr)
			}
		}
		if prev, ok := p.loadPersisted(fp, now); ok && !p.mem {
			// First fetch after a restart: no in-memory snapshot exists
			// yet, so serve the unexpired persisted observation and log
			// the failure instead of failing the refresh.
			slog.Warn("muse: quota fetch failed, serving persisted observation", "err", err)
			return prev, nil
		}
		return monitor.Snapshot{}, err
	}
	// A quota-less response must not wipe a previous observation: the
	// monitor keeps its snapshot when Fetch errors.
	if len(snap.Quotas) == 0 && (p.mem || p.hasPersisted(fp, now)) {
		return monitor.Snapshot{}, errors.New("Muse has not returned current quota data; showing the last known reading.")
	}
	if err := p.savePersisted(fp, snap); err != nil {
		slog.Warn("muse: quota observation not persisted", "err", err)
	}
	// A fresh reading lifts any earlier server backoff.
	_ = os.Remove(filepath.Join(dir, fp+".retry"))
	p.mem = true
	return snap, nil
}

// nextRetry reports the effective fetch hold: the later of the cooldown
// stamp and a persisted 429 retry time, when still in the future.
func (p *Provider) nextRetry(fp string, now time.Time) (time.Time, bool) {
	dir := p.cacheDir()
	var best time.Time
	ok := false
	if ts, found := readStamp(filepath.Join(dir, fp+".request")); found && now.Before(ts.Add(cooldown)) {
		best, ok = ts.Add(cooldown), true
	}
	if rt, found := readRetry(filepath.Join(dir, fp+".retry")); found && now.Before(rt) && (!ok || rt.After(best)) {
		best, ok = rt, true
	}
	return best, ok
}

// hasPersisted reports whether an unexpired observation is on disk.
func (p *Provider) hasPersisted(fp string, now time.Time) bool {
	_, ok := p.loadPersisted(fp, now)
	return ok
}

// readRetry parses a persisted 429 retry time (Unix nanos).
func readRetry(path string) (time.Time, bool) {
	data, err := creds.ReadFile(path)
	if err != nil {
		return time.Time{}, false
	}
	nanos, err := strconv.ParseInt(strings.TrimSpace(string(data)), 10, 64)
	if err != nil {
		return time.Time{}, false
	}
	return time.Unix(0, nanos).UTC(), true
}

// sharedClient refuses redirects of the token-bearing request. http.Client
// is safe for concurrent use.
var sharedClient = &http.Client{CheckRedirect: func(*http.Request, []*http.Request) error {
	return errRedirectRefused
}}

// request posts the fixed OAuth quota request and parses the response.
func (p *Provider) request(ctx context.Context, token string, now time.Time) (monitor.Snapshot, error) {
	rctx, cancel := context.WithTimeout(ctx, requestTimeout)
	defer cancel()
	req, err := http.NewRequestWithContext(rctx, http.MethodPost,
		strings.TrimSuffix(p.baseURL, "/")+"/muse-code/key", strings.NewReader(`{"onboard":false}`))
	if err != nil {
		return monitor.Snapshot{}, fmt.Errorf("Muse quota request failed: %w", err)
	}
	req.Header.Set("Authorization", "Bearer "+token)
	req.Header.Set("x-api-version", "1.0.0")
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "application/json")
	resp, err := sharedClient.Do(req)
	if err != nil {
		return monitor.Snapshot{}, fmt.Errorf("Muse quota request failed: %w", err)
	}
	defer resp.Body.Close()
	switch {
	case resp.StatusCode == http.StatusTooManyRequests:
		return monitor.Snapshot{}, &monitor.RateLimitedError{RetryAt: retryAt(resp.Header, now)}
	case resp.StatusCode == http.StatusUnauthorized || resp.StatusCode == http.StatusForbidden:
		return monitor.Snapshot{}, errors.New("Muse login rejected; sign in again with `muse login`.")
	case resp.StatusCode != http.StatusOK:
		return monitor.Snapshot{}, fmt.Errorf("Muse quota request failed with HTTP status %d", resp.StatusCode)
	}
	body, err := io.ReadAll(io.LimitReader(resp.Body, maxBodyBytes+1))
	if err != nil {
		return monitor.Snapshot{}, fmt.Errorf("Muse quota request failed: %w", err)
	}
	if len(body) > maxBodyBytes {
		return monitor.Snapshot{}, errors.New("Muse quota response exceeded the size limit")
	}
	return parseUsageResponse(body, now)
}

// retryAt honours Retry-After (seconds or HTTP date), else backs off one
// cooldown.
func retryAt(h http.Header, now time.Time) time.Time {
	if v := strings.TrimSpace(h.Get("Retry-After")); v != "" {
		if secs, err := strconv.Atoi(v); err == nil && secs >= 0 {
			return now.Add(time.Duration(secs) * time.Second)
		}
		if t, err := time.Parse(http.TimeFormat, v); err == nil && t.After(now) {
			return t
		}
	}
	return now.Add(cooldown)
}

// metaRecord is one providers.<name> entry of the CLI auth file.
type metaRecord struct {
	Mechanism   string `json:"mechanism"`
	Storage     string `json:"storage"`
	AccessToken string `json:"access_token"`
	APIKey      string `json:"api_key"`
}

var errUnavailable = errors.New("Muse OAuth credentials are missing or malformed. Verify the Muse CLI login and its auth metadata.")

// loadCredentials returns the OAuth access token, requiring the already
// issued standing api_key without ever issuing one.
func (p *Provider) loadCredentials() (string, error) {
	data, err := creds.ReadFile(p.authPath())
	if err != nil {
		// On darwin the login may live only in the Keychain; fall back
		// only for a missing file so a denied read cannot silently pick
		// a different login.
		if errors.Is(err, os.ErrNotExist) && p.goos == "darwin" {
			return p.credentialsFromKeychain()
		}
		if !errors.Is(err, os.ErrNotExist) {
			return "", fmt.Errorf("muse: credential file unreadable: %w", err)
		}
		return "", errUnavailable
	}
	if len(data) > maxCredentialBytes {
		return "", errUnavailable
	}
	var root struct {
		Providers map[string]metaRecord `json:"providers"`
	}
	if err := json.Unmarshal(data, &root); err != nil {
		return "", errUnavailable
	}
	meta, ok := root.Providers["meta"]
	if !ok || meta.Mechanism != "oauth" {
		return "", errUnavailable
	}
	if meta.Storage == "keychain" {
		return p.credentialsFromKeychain()
	}
	return checkCredentialPair(meta.AccessToken, meta.APIKey)
}

// credentialsFromKeychain reads the CLI's Keychain item (JSON with api_key
// and access_token) without ever prompting.
func (p *Provider) credentialsFromKeychain() (string, error) {
	raw, err := p.keychainRead("ai.meta.dev.credentials", "meta")
	switch {
	case errors.Is(err, ErrKeychainNotFound):
		return "", errors.New("Muse's Keychain credential item is missing. Complete Muse login in the CLI and verify its Keychain entry.")
	case errors.Is(err, ErrKeychainDenied):
		return "", errors.New("Muse credentials are in Keychain, but quotad needs Keychain access. Allow quotad to read the Muse credential item in Keychain Access, then refresh. quotad will not prompt for access.")
	case err != nil:
		return "", errors.New("Muse credentials could not be read from Keychain. Check Keychain availability and quotad's access to the Muse credential item.")
	}
	var rec metaRecord
	if len(raw) > maxCredentialBytes || json.Unmarshal(raw, &rec) != nil {
		return "", errors.New("Muse Keychain credentials are malformed. Complete or repair the Muse CLI login.")
	}
	return checkCredentialPair(rec.AccessToken, rec.APIKey)
}

// checkCredentialPair validates the token shape and the standing key.
func checkCredentialPair(token, apiKey string) (string, error) {
	if !strings.HasPrefix(token, "dca:") || len(token) <= len("dca:")+1 {
		return "", errors.New("Muse OAuth access token is missing or malformed. Complete the Muse CLI login.")
	}
	if strings.TrimSpace(apiKey) == "" {
		return "", errors.New("Muse OAuth login is present, but its standing API key is missing. Complete `muse login` in the Muse CLI; quotad will not issue the key itself.")
	}
	return token, nil
}

// tokenFingerprint keys per-login state without storing the token.
func tokenFingerprint(token string) string {
	sum := sha256.Sum256([]byte(token))
	return hex.EncodeToString(sum[:])
}

// authPath resolves the CLI auth file: $MUSE_AUTH_PATH when set (as the
// Swift loader checks first), else absolute $XDG_CONFIG_HOME/muse/auth.json,
// else ~/.config/muse/auth.json.
func (p *Provider) authPath() string {
	if override := p.lookupEnv("MUSE_AUTH_PATH"); override != "" {
		return override
	}
	if xdg := p.lookupEnv("XDG_CONFIG_HOME"); strings.HasPrefix(xdg, "/") {
		return filepath.Join(xdg, "muse/auth.json")
	}
	return filepath.Join(p.home(), ".config/muse/auth.json")
}

// cacheDir is ~/.quotabar/muse, shared with the Swift app's cooldown files.
func (p *Provider) cacheDir() string { return filepath.Join(p.home(), ".quotabar/muse") }

// home returns $HOME; tests always inject it, and the daemon runs with it set.
func (p *Provider) home() string { return p.lookupEnv("HOME") }

// readStamp parses a persisted cooldown stamp (Unix seconds from Go, or a
// float-double stamp written by Swift).
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

// unixFloat converts float Unix seconds to time, keeping the fractional
// part that int64 conversion would drop (Swift keeps it with
// Date(timeIntervalSince1970:)).
func unixFloat(secs float64) time.Time {
	whole := int64(secs)
	return time.Unix(whole, int64((secs-float64(whole))*1e9)).UTC()
}

// apiWindow is one quota window of the quota response.
type apiWindow struct {
	UsedPercent *float64 `json:"used_percent"`
	ResetsAt    *float64 `json:"resets_at"`
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

// parseUsageResponse decodes the quota response, ignoring returned secrets,
// email and other account fields. Missing windows stay unknown (no invented
// 100%), and windows whose reset already passed are omitted.
func parseUsageResponse(data []byte, now time.Time) (monitor.Snapshot, error) {
	var resp struct {
		IsSubsActive *bool   `json:"is_subs_active"`
		SubsTierName *string `json:"subs_tier_name"`
		SubsUsage    *struct {
			Window *apiWindow `json:"window"`
			Weekly *apiWindow `json:"weekly"`
		} `json:"subs_usage"`
	}
	if err := json.Unmarshal(data, &resp); err != nil || resp.IsSubsActive == nil {
		return monitor.Snapshot{}, errors.New("Muse quota response could not be decoded")
	}
	if !*resp.IsSubsActive {
		return monitor.Snapshot{}, errors.New("Muse subscription is not active.")
	}
	snap := monitor.Snapshot{ProviderID: "muse", CapturedAt: now}
	if resp.SubsTierName != nil {
		tier := monitor.CustomTier(*resp.SubsTierName)
		snap.Tier = &tier
	}
	if resp.SubsUsage == nil {
		return snap, nil
	}
	for _, win := range []struct {
		w   *apiWindow
		typ monitor.QuotaType
	}{{resp.SubsUsage.Window, monitor.SessionType()}, {resp.SubsUsage.Weekly, monitor.WeeklyType()}} {
		if win.w == nil {
			continue
		}
		var reset *time.Time
		if win.w.ResetsAt != nil {
			t := unixFloat(*win.w.ResetsAt)
			reset = &t
		}
		if q, ok := windowQuota(win.w.UsedPercent, reset, win.typ, now); ok {
			snap.Quotas = append(snap.Quotas, q)
		}
	}
	return snap, nil
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
func (p *Provider) loadPersisted(fp string, now time.Time) (monitor.Snapshot, bool) {
	data, err := creds.ReadFile(filepath.Join(p.cacheDir(), fp+".json"))
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
		if q, ok := windowQuota(&w.Remaining, reset, typ, now); ok {
			snap.Quotas = append(snap.Quotas, q)
		}
	}
	return snap, len(snap.Quotas) > 0
}

// savePersisted stores quota percentages and resets only: no tokens, keys,
// emails or other account data ever reach the cache directory.
func (p *Provider) savePersisted(fp string, snap monitor.Snapshot) error {
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
	return creds.WriteFileAtomic(filepath.Join(p.cacheDir(), fp+".json"), data)
}
