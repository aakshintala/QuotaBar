// Package claude reports Claude subscription quotas from Anthropic's OAuth
// usage API using the Claude Code OAuth credentials. It refreshes an expired
// access token (form-encoded, as CodexBar does) and writes the refreshed
// token back to the source it came from: the credentials file on every OS,
// or the "Claude Code-credentials" Keychain item on darwin.
package claude

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
	"net/url"
	"os"
	"os/user"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"time"

	"quotad/creds"
	"quotad/monitor"
)

const (
	providerID      = "claude"
	defaultUsageURL = "https://api.anthropic.com/api/oauth/usage"
	defaultRefresh  = "https://platform.claude.com/v1/oauth/token"
	// clientID is Claude Code's public OAuth client ID, not a secret.
	clientID          = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
	userAgent         = "claude-cli/2.1.0 (external, cli)"
	keychainService   = "Claude Code-credentials"
	defaultSnapTTL    = 300 * time.Second
	refreshBuffer     = 5 * time.Minute
	defaultRetryAfter = 5 * time.Minute
	minBackoff        = 5 * time.Minute
	maxBackoff        = 6 * time.Hour
	requestTimeout    = 15 * time.Second
	maxBody           = 1 << 20
)

// keychainRead and keychainWrite go through /usr/bin/security, the only tool
// the Claude Code Keychain item trusts. They are variables so tests can stub
// the security CLI without touching the real Keychain.
var (
	keychainRead  = creds.KeychainRead
	keychainWrite = creds.KeychainWrite
)

var (
	// errUnauthorized marks a 401/403 from usage: try one refresh first.
	errUnauthorized = errors.New("claude: unauthorized")
	// errInvalidGrant marks a revoked refresh token: stop refreshing it.
	errInvalidGrant = errors.New("claude: invalid_grant")
)

// reauthError never carries tokens or server output.
func reauthError() error {
	return errors.New("claude: token expired or revoked; log in to Claude Code again")
}

// credentials is one loaded OAuth record plus its raw JSON, so a writeback
// keeps every other field in the record unchanged.
type credentials struct {
	access       string
	refresh      string
	expiresAt    *float64
	subscription string
	viaKeychain  bool
	raw          map[string]any
}

// refreshState is the CodexBar failure gate for one refresh token: terminal
// on invalid_grant, else backoff from 5 minutes doubling to 6 hours.
type refreshState struct {
	terminal bool
	next     time.Time
	delay    time.Duration
	lastErr  error
}

// Provider fetches Claude quotas. The zero value is unusable; use New.
type Provider struct {
	home            string
	usageURL        string
	refreshURL      string
	client          *http.Client
	snapTTL         time.Duration
	now             func() time.Time
	useKeychain     bool
	keychainAccount string
	mu              sync.Mutex
	cached          monitor.Snapshot
	cachedAt        time.Time
	gateMu          sync.Mutex
	gates           map[string]*refreshState
}

// Option customizes a Provider for tests.
type Option func(*Provider)

// WithHome sets the home directory for credential and state paths.
func WithHome(dir string) Option { return func(p *Provider) { p.home = dir } }

// WithUsageURL overrides the usage endpoint (tests point it at httptest).
func WithUsageURL(u string) Option { return func(p *Provider) { p.usageURL = u } }

// WithRefreshURL overrides the token endpoint (tests point it at httptest).
func WithRefreshURL(u string) Option { return func(p *Provider) { p.refreshURL = u } }

// WithHTTPClient sets the HTTP client.
func WithHTTPClient(c *http.Client) Option { return func(p *Provider) { p.client = c } }

// WithSnapshotTTL overrides the 300 s snapshot cache TTL (0 disables it).
func WithSnapshotTTL(d time.Duration) Option { return func(p *Provider) { p.snapTTL = d } }

// WithNow overrides the clock (tests control expiry and backoff windows).
func WithNow(fn func() time.Time) Option { return func(p *Provider) { p.now = fn } }

// WithUseKeychain enables the Keychain fallback when the file is missing.
func WithUseKeychain(b bool) Option { return func(p *Provider) { p.useKeychain = b } }

// WithKeychainAccount sets the Keychain account instead of the login name.
func WithKeychainAccount(a string) Option { return func(p *Provider) { p.keychainAccount = a } }

// New builds a Claude provider with production defaults.
func New(opts ...Option) *Provider {
	home, _ := os.UserHomeDir()
	account := ""
	if u, err := user.Current(); err == nil {
		account = u.Username
	}
	p := &Provider{
		home: home, usageURL: defaultUsageURL, refreshURL: defaultRefresh,
		client: http.DefaultClient, snapTTL: defaultSnapTTL, now: time.Now,
		useKeychain: runtime.GOOS == "darwin", keychainAccount: account,
		gates: map[string]*refreshState{},
	}
	for _, opt := range opts {
		opt(p)
	}
	return p
}

// ID is the settings key and the feed id.
func (p *Provider) ID() string { return providerID }

// Available is cheap: the credentials file exists. No network, no Keychain.
func (p *Provider) Available() bool {
	_, err := os.Stat(filepath.Join(p.home, ".claude", ".credentials.json"))
	return err == nil
}

// Fetch returns the cached snapshot when fresh (even under force), refreshes
// an expiring token, and fetches usage.
func (p *Provider) Fetch(ctx context.Context) (monitor.Snapshot, error) {
	now := p.now()
	if snap, ok := p.cachedGet(now); ok {
		return snap, nil
	}
	c := p.load()
	if c == nil {
		return monitor.Snapshot{}, reauthError()
	}
	if until, ok := p.rateLimited(c.access, now); ok {
		return monitor.Snapshot{}, &monitor.RateLimitedError{RetryAt: until}
	}
	if needsRefresh(c, now) && c.refresh != "" {
		u, err := p.refreshNow(ctx, c, false)
		if err != nil {
			return monitor.Snapshot{}, err
		}
		c = u
	}
	body, err := p.getUsage(ctx, c.access)
	if errors.Is(err, errUnauthorized) && c.refresh != "" {
		u, rerr := p.refreshNow(ctx, c, true)
		if rerr != nil {
			return monitor.Snapshot{}, rerr
		}
		c = u
		body, err = p.getUsage(ctx, c.access)
	}
	if errors.Is(err, errUnauthorized) {
		return monitor.Snapshot{}, reauthError()
	}
	if err != nil {
		return monitor.Snapshot{}, err
	}
	quotas, tier, err := parseUsage(body, c.subscription)
	if err != nil {
		return monitor.Snapshot{}, err
	}
	snap := monitor.Snapshot{ProviderID: providerID, Quotas: quotas, CapturedAt: now, Tier: tier}
	p.cachedSet(snap, now)
	p.clearRate(c.access)
	return snap, nil
}

func (p *Provider) cachedGet(now time.Time) (monitor.Snapshot, bool) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.snapTTL <= 0 || p.cachedAt.IsZero() || now.Sub(p.cachedAt) >= p.snapTTL {
		return monitor.Snapshot{}, false
	}
	return p.cached, true
}

func (p *Provider) cachedSet(snap monitor.Snapshot, now time.Time) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.snapTTL <= 0 {
		return
	}
	p.cached, p.cachedAt = snap, now
}

// needsRefresh mirrors the Swift loader: missing expiry or within the 5
// minute buffer counts as needing a refresh.
func needsRefresh(c *credentials, now time.Time) bool {
	return c.expiresAt == nil || float64(now.UnixMilli())+float64(refreshBuffer.Milliseconds()) >= *c.expiresAt
}

// load reads the file first, then the Keychain item on darwin when the file
// is missing or unusable. It always rereads, so a CLI rotation is picked up.
func (p *Provider) load() *credentials {
	if data, err := creds.ReadFile(filepath.Join(p.home, ".claude", ".credentials.json")); err == nil {
		if c := parseCreds(data, false); c != nil {
			return c
		}
	}
	if !p.useKeychain || p.keychainAccount == "" {
		return nil
	}
	if data, err := keychainRead(keychainService, p.keychainAccount); err == nil && len(data) > 0 {
		return parseCreds(data, true)
	}
	return nil
}

func parseCreds(data []byte, viaKeychain bool) *credentials {
	var raw map[string]any
	if err := json.Unmarshal(data, &raw); err != nil {
		return nil
	}
	oauth, _ := raw["claudeAiOauth"].(map[string]any)
	access, _ := oauth["accessToken"].(string)
	if strings.TrimSpace(access) == "" {
		return nil
	}
	c := &credentials{access: strings.TrimSpace(access), viaKeychain: viaKeychain, raw: raw}
	c.refresh, _ = oauth["refreshToken"].(string)
	if ms, ok := oauth["expiresAt"].(float64); ok {
		c.expiresAt = &ms
	}
	c.subscription, _ = oauth["subscriptionType"].(string)
	return c
}

// save writes the refreshed token back to its source, keeping every other
// field unchanged. A save failure is logged, not returned: memory is fresh.
func (p *Provider) save(c *credentials) {
	sub, _ := c.raw["claudeAiOauth"].(map[string]any)
	if sub == nil {
		sub = map[string]any{}
		c.raw["claudeAiOauth"] = sub
	}
	sub["accessToken"] = c.access
	if c.refresh != "" {
		sub["refreshToken"] = c.refresh
	}
	if c.expiresAt != nil {
		sub["expiresAt"] = *c.expiresAt
	}
	data, err := json.Marshal(c.raw)
	if err != nil {
		slog.Warn("claude: failed to encode refreshed credentials")
		return
	}
	if !c.viaKeychain {
		if err := creds.WriteFileAtomic(filepath.Join(p.home, ".claude", ".credentials.json"), data); err != nil {
			slog.Warn("claude: failed to save refreshed token to file")
		}
	} else if err := keychainWrite(keychainService, p.keychainAccount, data); err != nil {
		slog.Warn("claude: failed to save refreshed token to Keychain")
	}
}

// refreshNow rereads the record, honors the failure gates, and redeems the
// refresh token. Unless forced (after a 401), fresh tokens are returned.
func (p *Provider) refreshNow(ctx context.Context, c *credentials, force bool) (*credentials, error) {
	if fresh := p.load(); fresh != nil && (fresh.access != c.access || fresh.refresh != c.refresh) {
		slog.Info("claude: picked up credentials rotated by Claude Code")
		c = fresh
	}
	if c.refresh == "" || (!force && !needsRefresh(c, p.now())) {
		return c, nil
	}
	updated, err := p.tryRefresh(ctx, c)
	if err != nil && !errors.Is(err, errInvalidGrant) {
		// The CLI may have rotated the file mid-redeem; reread once.
		if fresh := p.load(); fresh != nil && (fresh.access != c.access || fresh.refresh != c.refresh) {
			slog.Info("claude: retrying with credentials rotated during refresh")
			if fresh.refresh == "" || !needsRefresh(fresh, p.now()) {
				return fresh, nil
			}
			return p.tryRefresh(ctx, fresh)
		}
	}
	return updated, err
}

// tryRefresh checks the failure gate and redeems once, recording the CodexBar
// outcome: terminal on invalid_grant, doubling backoff otherwise.
func (p *Provider) tryRefresh(ctx context.Context, c *credentials) (*credentials, error) {
	key := tokenKey(c.refresh)
	p.gateMu.Lock()
	gate, ok := p.gates[key]
	if !ok {
		gate = &refreshState{}
		p.gates[key] = gate
	}
	if gate.terminal {
		p.gateMu.Unlock()
		return nil, reauthError()
	}
	if p.now().Before(gate.next) && !gate.next.IsZero() {
		err := gate.lastErr
		p.gateMu.Unlock()
		return nil, err
	}
	p.gateMu.Unlock()
	updated, err := p.redeem(ctx, c)
	p.gateMu.Lock()
	defer p.gateMu.Unlock()
	switch {
	case errors.Is(err, errInvalidGrant):
		gate.terminal = true
		return nil, reauthError()
	case err != nil:
		gate.delay = min(max(gate.delay*2, minBackoff), maxBackoff)
		gate.next, gate.lastErr = p.now().Add(gate.delay), err
		return nil, err
	default:
		delete(p.gates, key)
		return updated, nil
	}
}

// redeem POSTs the refresh with a form-encoded body and parses the reply.
func (p *Provider) redeem(ctx context.Context, c *credentials) (*credentials, error) {
	form := url.Values{"grant_type": {"refresh_token"}, "refresh_token": {c.refresh}, "client_id": {clientID}}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, p.refreshURL, strings.NewReader(form.Encode()))
	if err != nil {
		return nil, fmt.Errorf("claude: token refresh failed: %w", err)
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	body, status, _, err := p.call(req)
	if err != nil {
		return nil, fmt.Errorf("claude: token refresh failed: %w", err)
	}
	if status == http.StatusBadRequest || status == http.StatusUnauthorized {
		var fail struct {
			Error string `json:"error"`
		}
		if json.Unmarshal(body, &fail) == nil && fail.Error == "invalid_grant" {
			return nil, errInvalidGrant
		}
		return nil, fmt.Errorf("claude: token refresh failed: HTTP %d", status)
	}
	if status < 200 || status >= 300 {
		return nil, fmt.Errorf("claude: token refresh failed: HTTP %d", status)
	}
	var ok struct {
		AccessToken  string   `json:"access_token"`
		RefreshToken *string  `json:"refresh_token"`
		ExpiresIn    *float64 `json:"expires_in"`
	}
	if json.Unmarshal(body, &ok) != nil || strings.TrimSpace(ok.AccessToken) == "" {
		return nil, errors.New("claude: token refresh failed: no access token in response")
	}
	updated := &credentials{access: strings.TrimSpace(ok.AccessToken), refresh: c.refresh,
		expiresAt: c.expiresAt, subscription: c.subscription, viaKeychain: c.viaKeychain, raw: c.raw}
	if ok.RefreshToken != nil && *ok.RefreshToken != "" {
		updated.refresh = *ok.RefreshToken
	}
	if ok.ExpiresIn != nil {
		ms := float64(p.now().UnixMilli()) + *ok.ExpiresIn*1000
		updated.expiresAt = &ms
	}
	p.save(updated)
	slog.Info("claude: token refreshed")
	return updated, nil
}

// getUsage calls the usage endpoint the way Swift does, plus ?cedar_ember=1
// and the claude-cli User-Agent. A 429 records the Retry-After gate and
// returns a *monitor.RateLimitedError; 401/403 returns errUnauthorized.
func (p *Provider) getUsage(ctx context.Context, access string) ([]byte, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, p.usageURL+"?cedar_ember=1", nil)
	if err != nil {
		return nil, fmt.Errorf("claude: usage request failed: %w", err)
	}
	req.Header.Set("Authorization", "Bearer "+strings.TrimSpace(access))
	req.Header.Set("Accept", "application/json")
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("anthropic-beta", "oauth-2025-04-20")
	req.Header.Set("User-Agent", userAgent)
	body, status, header, err := p.call(req)
	if err != nil {
		return nil, fmt.Errorf("claude: usage request failed: %w", err)
	}
	switch {
	case status == http.StatusOK:
		return body, nil
	case status == http.StatusTooManyRequests:
		retryAt := p.noteRateLimit(access, header.Get("Retry-After"))
		slog.Info("claude: rate limited", "retryAt", retryAt.Format(time.RFC3339))
		return nil, &monitor.RateLimitedError{RetryAt: retryAt}
	case status == http.StatusUnauthorized || status == http.StatusForbidden:
		return nil, errUnauthorized
	default:
		return nil, fmt.Errorf("claude: usage request failed: HTTP %d", status)
	}
}

// call runs one request with the caller's context plus a per-request timeout,
// reading at most 1 MiB of the body.
func (p *Provider) call(req *http.Request) (body []byte, status int, header http.Header, err error) {
	ctx, cancel := context.WithTimeout(req.Context(), requestTimeout)
	defer cancel()
	resp, err := p.client.Do(req.WithContext(ctx))
	if err != nil {
		return nil, 0, nil, err
	}
	defer resp.Body.Close()
	body, err = io.ReadAll(io.LimitReader(resp.Body, maxBody))
	return body, resp.StatusCode, resp.Header, err
}

// tokenKey anonymizes a token for gate maps: a SHA-256 hex prefix.
func tokenKey(token string) string {
	sum := sha256.Sum256([]byte(token))
	return hex.EncodeToString(sum[:])[:16]
}

func (p *Provider) ratePath() string {
	return filepath.Join(p.home, ".quotabar", "claude", "ratelimit.json")
}

// readRateFile returns the persisted gates; missing or corrupt files gate
// nothing, so a restart never hammers the endpoint on stale state.
func (p *Provider) readRateFile() map[string]time.Time {
	out := map[string]time.Time{}
	if data, err := creds.ReadFile(p.ratePath()); err == nil {
		_ = json.Unmarshal(data, &out)
	}
	return out
}

func (p *Provider) writeRateFile(gates map[string]time.Time) {
	data, err := json.Marshal(gates)
	if err != nil {
		return
	}
	if err := os.MkdirAll(filepath.Dir(p.ratePath()), 0o700); err != nil {
		slog.Warn("claude: failed to create rate-limit directory")
		return
	}
	if err := creds.WriteFileAtomic(p.ratePath(), data); err != nil {
		slog.Warn("claude: failed to persist rate-limit gate")
	}
}

// rateLimited reports whether the token prefix is still gated, in the file
// that survives restarts.
func (p *Provider) rateLimited(access string, now time.Time) (time.Time, bool) {
	if until, ok := p.readRateFile()[tokenKey(access)]; ok && now.Before(until) {
		return until, true
	}
	return time.Time{}, false
}

// noteRateLimit records the 429 gate (Retry-After or the 5 minute default,
// keeping the longer of the current gate and the new one) and persists it.
func (p *Provider) noteRateLimit(access, header string) time.Time {
	retryAt := p.now().Add(defaultRetryAfter)
	if d, ok := parseRetryAfter(header, p.now()); ok {
		retryAt = p.now().Add(d)
	}
	gates, key := p.readRateFile(), tokenKey(access)
	if cur, ok := gates[key]; ok && cur.After(retryAt) {
		retryAt = cur
	}
	gates[key] = retryAt
	p.writeRateFile(gates)
	return retryAt
}

// clearRate drops the token prefix from the persisted gate after a success.
func (p *Provider) clearRate(access string) {
	gates, key := p.readRateFile(), tokenKey(access)
	if _, ok := gates[key]; !ok {
		return
	}
	delete(gates, key)
	p.writeRateFile(gates)
}

// usageResponse mirrors the /api/oauth/usage body. Spend shapes are decoded
// but intentionally dropped: the Swift feed exposes no cost row either, and
// the Go feed has no cost field.
type usageResponse struct {
	FiveHour       *quotaWindow `json:"five_hour"`
	SevenDay       *quotaWindow `json:"seven_day"`
	SevenDaySonnet *quotaWindow `json:"seven_day_sonnet"`
	SevenDayOpus   *quotaWindow `json:"seven_day_opus"`
	Limits         []limitEntry `json:"limits"`
}

type quotaWindow struct {
	Utilization *float64 `json:"utilization"`
	ResetsAt    *string  `json:"resets_at"`
}

type limitEntry struct {
	Kind    *string  `json:"kind"`
	Percent *float64 `json:"percent"`
	Resets  *string  `json:"resets_at"`
	Scope   *struct {
		Model *struct {
			DisplayName *string `json:"display_name"`
		} `json:"model"`
	} `json:"scope"`
}

// parseUsage builds percent quotas from the legacy windows and the generic
// limits array, skipping malformed scoped entries, suppressing duplicates,
// and keeping over-quota negative remaining values.
func parseUsage(data []byte, subscription string) ([]monitor.Quota, *monitor.Tier, error) {
	var body usageResponse
	if err := json.Unmarshal(data, &body); err != nil {
		return nil, nil, fmt.Errorf("claude: failed to parse usage response: %w", err)
	}
	var quotas []monitor.Quota
	seen := map[monitor.QuotaType]bool{}
	add := func(window *quotaWindow, typ monitor.QuotaType) {
		if window == nil || window.Utilization == nil || seen[typ] {
			return
		}
		seen[typ] = true
		q := monitor.NewPercentQuota(100-*window.Utilization, typ, providerID)
		q.ResetsAt = parseTime(window.ResetsAt)
		quotas = append(quotas, q)
	}
	add(body.FiveHour, monitor.SessionType())
	add(body.SevenDay, monitor.WeeklyType())
	add(body.SevenDaySonnet, monitor.ModelType("sonnet"))
	add(body.SevenDayOpus, monitor.ModelType("opus"))
	for _, entry := range body.Limits {
		if entry.Kind == nil || *entry.Kind != "weekly_scoped" || entry.Percent == nil {
			continue
		}
		name := ""
		if entry.Scope != nil && entry.Scope.Model != nil && entry.Scope.Model.DisplayName != nil {
			if words := strings.Fields(strings.ToLower(*entry.Scope.Model.DisplayName)); len(words) > 0 {
				name = words[0]
			}
		}
		if name == "" || seen[monitor.ModelType(name)] {
			continue
		}
		add(&quotaWindow{Utilization: entry.Percent, ResetsAt: entry.Resets}, monitor.ModelType(name))
	}
	return quotas, tierFor(subscription), nil
}

// tierFor maps the subscription type the way the Swift probe does.
func tierFor(subscription string) *monitor.Tier {
	switch strings.ToLower(subscription) {
	case "claude_max", "max":
		t := monitor.ClaudeMaxTier()
		return &t
	case "claude_pro", "pro":
		t := monitor.ClaudeProTier()
		return &t
	case "api", "claude_api":
		t := monitor.ClaudeAPITier()
		return &t
	case "":
		return nil
	default:
		t := monitor.CustomTier(subscription)
		return &t
	}
}

// parseTime parses ISO-8601 with or without fractional seconds.
func parseTime(s *string) *time.Time {
	if s == nil {
		return nil
	}
	for _, layout := range []string{time.RFC3339Nano, "2006-01-02T15:04:05.999999999Z0700", "2006-01-02T15:04:05Z0700"} {
		if t, err := time.Parse(layout, *s); err == nil {
			return &t
		}
	}
	return nil
}

// parseRetryAfter parses an HTTP Retry-After value: positive seconds, or a
// future HTTP date. Zero, negative, missing, malformed and past values are
// rejected so the caller applies its fallback (the endpoint has returned
// Retry-After: 0 while still 429ing).
func parseRetryAfter(value string, now time.Time) (time.Duration, bool) {
	if s := strings.TrimSpace(value); s != "" {
		if f, err := strconv.ParseFloat(s, 64); err == nil {
			return time.Duration(f * float64(time.Second)), f > 0
		}
		for _, layout := range []string{time.RFC1123, time.RFC1123Z} {
			if date, err := time.Parse(layout, s); err == nil {
				return date.Sub(now), date.After(now)
			}
		}
	}
	return 0, false
}
