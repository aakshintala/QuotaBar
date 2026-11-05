// Package codex probes the Codex usage API with the Codex CLI's OAuth
// credentials, refreshing them the way the Swift app does. The session and
// weekly windows are told apart by their length (300 vs 10080 minutes),
// not by their position in the response.
package codex

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"math"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"quotad/creds"
	"quotad/monitor"
)

const (
	// ID is the settings key and the feed id.
	ID = "codex"
	// defaultUsageURL is the ChatGPT backend usage endpoint.
	defaultUsageURL = "https://chatgpt.com/backend-api/wham/usage"
	// defaultRefreshURL is the OpenAI OAuth token endpoint.
	defaultRefreshURL = "https://auth.openai.com/oauth/token"
	// clientID is Codex's public OAuth client id, not a secret.
	clientID = "app_EMoamEEZ73f0CkXaXp7hrann"
	// defaultTimeout bounds one HTTP request.
	defaultTimeout = 15 * time.Second
	// refreshAge is the last_refresh staleness threshold, as in Swift.
	refreshAge = 8 * 24 * time.Hour
	// jwtRefreshWindow refreshes a JWT access token this close to expiry.
	jwtRefreshWindow = 5 * time.Minute
	// maxBody caps one response body read.
	maxBody = 1 << 20
	// rateLimitFallback applies when 429 carries no usable Retry-After.
	rateLimitFallback = 5 * time.Minute
)

// errNeedsLogin tells the user to log in again. It carries no secret.
var errNeedsLogin = errors.New("codex: not logged in; run `codex` in the terminal to log in again")

// Config carries the seams tests need. The zero value is production.
type Config struct {
	UsageURL   string
	RefreshURL string
	Client     *http.Client
	LookupEnv  func(string) string
	HomeDir    string
	Now        func() time.Time
}

// Provider fetches Codex quota.
type Provider struct {
	usageURL   string
	refreshURL string
	client     *http.Client
	lookupEnv  func(string) string
	homeDir    string
	now        func() time.Time
}

// New builds a Codex Provider.
func New(c Config) *Provider {
	if c.UsageURL == "" {
		c.UsageURL = defaultUsageURL
	}
	if c.RefreshURL == "" {
		c.RefreshURL = defaultRefreshURL
	}
	if c.Client == nil {
		c.Client = &http.Client{}
	}
	if c.LookupEnv == nil {
		c.LookupEnv = os.Getenv
	}
	if c.Now == nil {
		c.Now = time.Now
	}
	return &Provider{
		usageURL: c.UsageURL, refreshURL: c.RefreshURL, client: c.Client,
		lookupEnv: c.LookupEnv, homeDir: c.HomeDir, now: c.Now,
	}
}

// ID returns the provider id.
func (p *Provider) ID() string { return ID }

// AuthPath resolves the credential file: $CODEX_HOME/auth.json, else
// ~/.codex/auth.json. Same on Linux and Mac.
func (p *Provider) AuthPath() string {
	if dir := p.lookupEnv("CODEX_HOME"); dir != "" {
		return filepath.Join(dir, "auth.json")
	}
	home := p.homeDir
	if home == "" {
		home, _ = os.UserHomeDir()
	}
	return filepath.Join(home, ".codex", "auth.json")
}

// Available is cheap: it only stats the credential file.
func (p *Provider) Available() bool {
	_, err := os.Stat(p.AuthPath())
	return err == nil
}

// credentials is one auth.json read.
type credentials struct {
	accessToken  string
	refreshToken string
	accountID    string
	lastRefresh  string
}

// loadCredentials reads and parses the auth file. It returns nil when no
// usable OAuth credentials exist, as the Swift loader does.
func (p *Provider) loadCredentials() *credentials {
	data, err := creds.ReadFile(p.AuthPath())
	if err != nil {
		return nil
	}
	var raw map[string]any
	if err := json.Unmarshal(data, &raw); err != nil {
		return nil
	}
	tokens, _ := raw["tokens"].(map[string]any)
	access, _ := tokens["access_token"].(string)
	if access == "" {
		return nil
	}
	refresh, _ := tokens["refresh_token"].(string)
	account, _ := tokens["account_id"].(string)
	last, _ := raw["last_refresh"].(string)
	return &credentials{accessToken: access, refreshToken: refresh, accountID: account, lastRefresh: last}
}

// jwtExpiry returns the access token's exp claim when it is a JWT carrying one.
func jwtExpiry(token string) (time.Time, bool) {
	parts := strings.Split(token, ".")
	if len(parts) != 3 {
		return time.Time{}, false
	}
	payload, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		return time.Time{}, false
	}
	var claims struct {
		Exp *float64 `json:"exp"`
	}
	if err := json.Unmarshal(payload, &claims); err != nil || claims.Exp == nil {
		return time.Time{}, false
	}
	return time.Unix(int64(*claims.Exp), 0), true
}

// needsRefresh reports whether the access token should be refreshed before
// use: from its JWT expiry when the claim is present, else from the age of
// last_refresh, as the Swift loader does. A missing or unparseable
// last_refresh parses as the zero time, which is always stale.
func needsRefresh(accessToken, lastRefresh string, now time.Time) bool {
	if exp, ok := jwtExpiry(accessToken); ok {
		return !exp.After(now.Add(jwtRefreshWindow))
	}
	last, _ := time.Parse(time.RFC3339Nano, lastRefresh)
	return now.Sub(last) > refreshAge
}

// Fetch returns the current Codex snapshot, refreshing the token first when
// it is stale and once more on a 401 before giving up.
func (p *Provider) Fetch(ctx context.Context) (monitor.Snapshot, error) {
	c := p.loadCredentials()
	if c == nil {
		return monitor.Snapshot{}, errNeedsLogin
	}
	if needsRefresh(c.accessToken, c.lastRefresh, p.now()) {
		// Reread first: the Codex CLI may have rotated the file already.
		if fresh := p.loadCredentials(); fresh != nil && fresh.accessToken != c.accessToken {
			c = fresh
		}
		if needsRefresh(c.accessToken, c.lastRefresh, p.now()) {
			if u, rerr := p.refresh(ctx, c); rerr != nil {
				if errors.Is(rerr, errNeedsLogin) {
					return monitor.Snapshot{}, rerr
				}
				slog.Warn("codex: token refresh failed, trying existing token")
			} else {
				c = u
			}
		}
	}
	body, header, status, err := p.getUsage(ctx, c)
	if err != nil {
		return monitor.Snapshot{}, err
	}
	if (status == http.StatusUnauthorized || status == http.StatusForbidden) && c.refreshToken != "" {
		u, rerr := p.refresh(ctx, c)
		if rerr != nil {
			if errors.Is(rerr, errNeedsLogin) {
				return monitor.Snapshot{}, rerr
			}
			return monitor.Snapshot{}, fmt.Errorf("codex: usage request failed (HTTP %d)", status)
		}
		if body, header, status, err = p.getUsage(ctx, u); err != nil {
			return monitor.Snapshot{}, err
		}
		c = u
	}
	if status == http.StatusTooManyRequests {
		return monitor.Snapshot{}, &monitor.RateLimitedError{RetryAt: retryAfter(header, p.now())}
	}
	if status == http.StatusUnauthorized || status == http.StatusForbidden {
		return monitor.Snapshot{}, errNeedsLogin
	}
	if status != http.StatusOK {
		return monitor.Snapshot{}, fmt.Errorf("codex: usage request failed (HTTP %d)", status)
	}
	quotas, tier, err := parseUsage(body, header, p.now())
	if err != nil {
		return monitor.Snapshot{}, err
	}
	return monitor.Snapshot{ProviderID: ID, Quotas: quotas, CapturedAt: p.now(), Tier: tier}, nil
}

// roundTrip sends one request with the caller's context plus a per-request
// timeout, returning at most 1 MiB of the body.
func (p *Provider) roundTrip(ctx context.Context, req *http.Request) ([]byte, http.Header, int, error) {
	rctx, cancel := context.WithTimeout(ctx, defaultTimeout)
	defer cancel()
	resp, err := p.client.Do(req.WithContext(rctx))
	if err != nil {
		return nil, nil, 0, fmt.Errorf("codex: request failed: %w", err)
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(io.LimitReader(resp.Body, maxBody))
	if err != nil {
		return nil, nil, 0, fmt.Errorf("codex: read response: %w", err)
	}
	return body, resp.Header, resp.StatusCode, nil
}

// getUsage performs one authenticated usage request.
func (p *Provider) getUsage(ctx context.Context, c *credentials) ([]byte, http.Header, int, error) {
	req, err := http.NewRequest(http.MethodGet, p.usageURL, nil)
	if err != nil {
		return nil, nil, 0, fmt.Errorf("codex: build usage request: %w", err)
	}
	req.Header.Set("Authorization", "Bearer "+c.accessToken)
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", "OpenUsage")
	if c.accountID != "" {
		req.Header.Set("ChatGPT-Account-Id", c.accountID)
	}
	return p.roundTrip(ctx, req)
}

// refresh redeems the refresh token and writes the new credentials back.
func (p *Provider) refresh(ctx context.Context, c *credentials) (*credentials, error) {
	if c.refreshToken == "" {
		return nil, errNeedsLogin
	}
	form := url.Values{"grant_type": {"refresh_token"}, "client_id": {clientID}, "refresh_token": {c.refreshToken}}
	req, err := http.NewRequest(http.MethodPost, p.refreshURL, strings.NewReader(form.Encode()))
	if err != nil {
		return nil, fmt.Errorf("codex: build refresh request: %w", err)
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	body, _, status, err := p.roundTrip(ctx, req)
	if err != nil {
		return nil, err
	}
	if status == http.StatusBadRequest || status == http.StatusUnauthorized {
		slog.Error("codex: refresh rejected, session expired", "status", status)
		return nil, errNeedsLogin
	}
	if status < 200 || status >= 300 {
		return nil, fmt.Errorf("codex: token refresh failed (HTTP %d)", status)
	}
	var doc struct {
		Access  string `json:"access_token"`
		Refresh string `json:"refresh_token"`
		IDToken string `json:"id_token"`
	}
	if err := json.Unmarshal(body, &doc); err != nil || doc.Access == "" {
		return nil, fmt.Errorf("codex: no access token in refresh response")
	}
	updated := &credentials{accessToken: doc.Access, refreshToken: c.refreshToken, accountID: c.accountID}
	if doc.Refresh != "" {
		updated.refreshToken = doc.Refresh
	}
	p.saveRefreshed(updated, doc.IDToken)
	slog.Info("codex: token refreshed")
	return updated, nil
}

// saveRefreshed merges the new tokens into the latest on-disk JSON and
// writes it back atomically, keeping every other field unchanged.
func (p *Provider) saveRefreshed(updated *credentials, idToken string) {
	merged := map[string]any{}
	if disk, err := creds.ReadFile(p.AuthPath()); err == nil {
		_ = json.Unmarshal(disk, &merged)
	}
	if merged == nil {
		merged = map[string]any{}
	}
	tokens, _ := merged["tokens"].(map[string]any)
	if tokens == nil {
		tokens = map[string]any{}
	}
	tokens["access_token"] = updated.accessToken
	tokens["refresh_token"] = updated.refreshToken
	if idToken != "" {
		tokens["id_token"] = idToken
	}
	if _, ok := tokens["account_id"]; !ok && updated.accountID != "" {
		tokens["account_id"] = updated.accountID
	}
	merged["tokens"] = tokens
	merged["last_refresh"] = p.now().UTC().Format(time.RFC3339)
	data, err := json.MarshalIndent(merged, "", "  ")
	if err != nil {
		slog.Warn("codex: encode refreshed credentials failed")
		return
	}
	if err := creds.WriteFileAtomic(p.AuthPath(), data); err != nil {
		slog.Warn("codex: save refreshed credentials failed")
	}
}

// windowResponse is one rate-limit window. JSON numbers decode as float64
// whether the API spells them int or float.
type windowResponse struct {
	UsedPercent   *float64 `json:"used_percent"`
	ResetAt       *float64 `json:"reset_at"`
	ResetAfter    *float64 `json:"reset_after_seconds"`
	LengthSeconds *float64 `json:"limit_window_seconds"`
}

// usageResponse is the wham/usage body subset QuotaBar reads.
type usageResponse struct {
	RateLimit *struct {
		Primary   *windowResponse `json:"primary_window"`
		Secondary *windowResponse `json:"secondary_window"`
	} `json:"rate_limit"`
	Credits *struct {
		HasCredits *bool `json:"has_credits"`
		Balance    any   `json:"balance"`
	} `json:"credits"`
	PlanType string `json:"plan_type"`
}

// roleOf maps a window length to its session or weekly role, as the CodexBar
// normalizer does. The lengths come from the domain window durations.
func roleOf(w *windowResponse) monitor.QuotaType {
	if w.LengthSeconds != nil {
		switch *w.LengthSeconds {
		case float64(monitor.SessionType().Duration().Seconds()):
			return monitor.SessionType()
		case float64(monitor.WeeklyType().Duration().Seconds()):
			return monitor.WeeklyType()
		}
	}
	return monitor.QuotaType{}
}

// parseUsage maps the usage response onto quotas. Header percents win over
// the body; each window's role comes from its length, falling back to its
// position when the length is absent.
func parseUsage(body []byte, header http.Header, now time.Time) ([]monitor.Quota, *monitor.Tier, error) {
	var doc usageResponse
	if err := json.Unmarshal(body, &doc); err != nil {
		return nil, nil, fmt.Errorf("codex: parse usage response: %w", err)
	}
	type slot struct {
		typ  monitor.QuotaType
		name string
		win  *windowResponse
	}
	session, weekly := monitor.SessionType(), monitor.WeeklyType()
	var slots []slot
	if doc.RateLimit != nil {
		if w := doc.RateLimit.Primary; w != nil {
			slots = append(slots, slot{session, "primary", w})
		}
		if w := doc.RateLimit.Secondary; w != nil {
			slots = append(slots, slot{weekly, "secondary", w})
		}
	}
	// A window whose length claims the other slot swaps places with it; a
	// lone window takes the slot its length names.
	if len(slots) == 2 && roleOf(slots[0].win) == weekly && roleOf(slots[1].win) != weekly {
		slots[0], slots[1] = slots[1], slots[0]
		slots[0].typ, slots[1].typ = session, weekly
	}
	if len(slots) == 1 {
		if r := roleOf(slots[0].win); r != (monitor.QuotaType{}) {
			slots[0].typ = r
		}
	}
	var quotas []monitor.Quota
	for _, s := range slots {
		used, ok := headerPercent(header, "x-codex-"+s.name+"-used-percent")
		if !ok {
			if s.win.UsedPercent == nil {
				continue
			}
			used = *s.win.UsedPercent
		}
		q := monitor.NewPercentQuota(max(0, 100-used), s.typ, ID)
		switch {
		case s.win.ResetAt != nil:
			t := time.Unix(int64(*s.win.ResetAt), 0)
			q.ResetsAt = &t
		case s.win.ResetAfter != nil:
			t := now.Add(time.Duration(*s.win.ResetAfter * float64(time.Second)))
			q.ResetsAt = &t
		}
		quotas = append(quotas, q)
	}
	if balance, ok := creditsBalance(doc.Credits, header); ok {
		unit := monitor.Credits
		quotas = append(quotas, monitor.Quota{
			Type: monitor.TimeLimitType("Credits"), ProviderID: ID,
			BalanceRemaining: &balance, BalanceUnit: &unit,
		})
	}
	var tier *monitor.Tier
	if doc.PlanType != "" {
		t := monitor.CustomTier(strings.ToUpper(doc.PlanType))
		tier = &t
	}
	return quotas, tier, nil
}

// creditsBalance resolves the credits balance, header first then body, and
// reports whether the Credits bucket applies: a positive balance with
// has_credits not false. The body balance may be a number or a string.
func creditsBalance(c *struct {
	HasCredits *bool `json:"has_credits"`
	Balance    any   `json:"balance"`
}, header http.Header) (float64, bool) {
	var balance *float64
	if v, ok := headerPercent(header, "x-codex-credits-balance"); ok {
		balance = &v
	} else if c != nil {
		switch b := c.Balance.(type) {
		case float64:
			balance = &b
		case string:
			if v, err := strconv.ParseFloat(strings.TrimSpace(b), 64); err == nil {
				balance = &v
			}
		}
	}
	if balance == nil || *balance <= 0 {
		return 0, false
	}
	if c != nil && c.HasCredits != nil && !*c.HasCredits {
		return 0, false
	}
	return *balance, true
}

// headerPercent parses a percent response header.
func headerPercent(header http.Header, key string) (float64, bool) {
	s := strings.TrimSpace(header.Get(key))
	if s == "" {
		return 0, false
	}
	f, err := strconv.ParseFloat(s, 64)
	if err != nil || math.IsNaN(f) || math.IsInf(f, 0) {
		return 0, false
	}
	return f, true
}

// retryAfter honors Retry-After (seconds or an HTTP date), with a fallback.
func retryAfter(header http.Header, now time.Time) time.Time {
	if s := strings.TrimSpace(header.Get("Retry-After")); s != "" {
		if secs, err := strconv.Atoi(s); err == nil {
			if secs > 0 {
				return now.Add(time.Duration(secs) * time.Second)
			}
		} else if t, err := time.Parse(http.TimeFormat, s); err == nil && t.After(now) {
			return t
		}
	}
	return now.Add(rateLimitFallback)
}
