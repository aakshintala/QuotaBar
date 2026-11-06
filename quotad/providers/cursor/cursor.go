// Package cursor probes Cursor's usage-summary API with cookie auth built
// from the local access token. Credentials are read-only: the Cursor app
// database is queried through the sqlite3 CLI, falling back to the
// cursor-agent auth file. The Cursor CLI is never invoked.
package cursor

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
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"time"

	"quotad/monitor"
)

const (
	defaultBaseURL = "https://cursor.com"
	usagePath      = "/api/usage-summary"
	sqliteQuery    = "PRAGMA busy_timeout=250; SELECT value FROM ItemTable WHERE key='cursorAuth/accessToken'"
	maxBodyBytes   = 1 << 20
	requestTimeout = 15 * time.Second
	fallbackRetry  = 5 * time.Minute
)

// runCommand executes a process and returns its stdout. It is a variable so
// tests can stub the sqlite3 CLI. The query holds no secret, so passing it
// on argv is fine; the token comes back on stdout and is never logged.
var runCommand = func(ctx context.Context, name string, args ...string) ([]byte, error) {
	return exec.CommandContext(ctx, name, args...).Output()
}

var (
	errAuth        = errors.New("cursor: unauthorized")
	errLogin       = errors.New("cursor: not logged in; log in to the Cursor app or CLI again")
	errSafeStorage = errors.New("cursor: database token uses unsupported cursor-safe-storage encryption")
)

// Provider fetches Cursor quotas. Tests inject paths, OS, endpoint and
// client through options; the zero value plus New() uses live defaults.
type Provider struct {
	client      *http.Client
	baseURL     string
	goos        string
	home        string
	xdgConfig   string
	mu          sync.Mutex
	cachedToken string
	cachedUser  string
	cachedExp   time.Time
	hasCache    bool
}

// Option customizes a Provider.
type Option func(*Provider)

func WithHTTPClient(c *http.Client) Option { return func(p *Provider) { p.client = c } }
func WithBaseURL(u string) Option          { return func(p *Provider) { p.baseURL = u } }
func WithGOOS(s string) Option             { return func(p *Provider) { p.goos = s } }
func WithHome(h string) Option             { return func(p *Provider) { p.home = h } }
func WithXDGConfigHome(x string) Option    { return func(p *Provider) { p.xdgConfig = x } }

// New builds a Cursor provider.
func New(opts ...Option) *Provider {
	p := &Provider{baseURL: defaultBaseURL, client: http.DefaultClient}
	for _, opt := range opts {
		opt(p)
	}
	return p
}

// ID is the settings key and feed id.
func (p *Provider) ID() string { return "cursor" }

func (p *Provider) homeDir() string {
	if p.home != "" {
		return p.home
	}
	home, _ := os.UserHomeDir()
	return home
}

func (p *Provider) dbPath() string {
	home := p.homeDir()
	goos := p.goos
	if goos == "" {
		goos = runtime.GOOS
	}
	if goos == "darwin" {
		return filepath.Join(home, "Library", "Application Support", "Cursor", "User", "globalStorage", "state.vscdb")
	}
	xdg := p.xdgConfig
	if xdg == "" {
		if xdg = os.Getenv("XDG_CONFIG_HOME"); !filepath.IsAbs(xdg) {
			xdg = filepath.Join(home, ".config")
		}
	}
	return filepath.Join(xdg, "Cursor", "User", "globalStorage", "state.vscdb")
}

func (p *Provider) authFilePath() string {
	return filepath.Join(p.homeDir(), ".config", "cursor", "auth.json")
}

// Available is cheap: it only stats the credential sources.
func (p *Provider) Available() bool {
	_, dbErr := os.Stat(p.dbPath())
	_, authErr := os.Stat(p.authFilePath())
	return dbErr == nil || authErr == nil
}

// Fetch reads credentials (cached while the JWT is unexpired), calls
// usage-summary, and on 401/403 rereads the sources once before giving up
// with a log-in error.
func (p *Provider) Fetch(ctx context.Context) (monitor.Snapshot, error) {
	token, user, err := p.credentials(ctx, false)
	if err != nil {
		return monitor.Snapshot{}, err
	}
	if snap, err := p.fetchOnce(ctx, token, user); !errors.Is(err, errAuth) {
		return snap, err
	}
	slog.Debug("cursor: unauthorized, rereading credentials once")
	token, user, err = p.credentials(ctx, true)
	if err != nil {
		if ctx.Err() != nil {
			return monitor.Snapshot{}, ctx.Err()
		}
		if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
			return monitor.Snapshot{}, err
		}
		return monitor.Snapshot{}, errLogin
	}
	snap, err := p.fetchOnce(ctx, token, user)
	if errors.Is(err, errAuth) {
		return monitor.Snapshot{}, errLogin
	}
	return snap, err
}

// credentials returns the cached token while its JWT is unexpired, else
// reads the sources with first hit winning. force skips the cache.
func (p *Provider) credentials(ctx context.Context, force bool) (string, string, error) {
	p.mu.Lock()
	cached, user, exp, ok := p.cachedToken, p.cachedUser, p.cachedExp, p.hasCache
	if force {
		p.hasCache = false
	}
	p.mu.Unlock()
	if !force && ok && time.Now().Before(exp) {
		return cached, user, nil
	}
	token, err := p.readToken(ctx)
	if err != nil {
		return "", "", err
	}
	sub, jwtExp, err := decodeJWT(token)
	if err != nil {
		return "", "", err
	}
	user, err = cookieUserID(sub)
	if err != nil {
		return "", "", err
	}
	if jwtExp != nil {
		p.mu.Lock()
		p.cachedToken, p.cachedUser, p.cachedExp, p.hasCache = token, user, *jwtExp, true
		p.mu.Unlock()
	}
	return token, user, nil
}

// readToken tries the app database first, then the cursor-agent auth file.
func (p *Provider) readToken(ctx context.Context) (string, error) {
	token, storErr := p.dbToken(ctx)
	if storErr != nil && !errors.Is(storErr, errSafeStorage) {
		return "", storErr
	}
	if token != "" {
		return token, nil
	}
	if token, ok := p.agentToken(); ok {
		return token, nil
	}
	if storErr != nil {
		return "", storErr
	}
	return "", errLogin
}

// dbToken queries state.vscdb read-only through the sqlite3 CLI. A missing
// database, a missing key or an encrypted value falls through to the next
// source; anything else is an error.
func (p *Provider) dbToken(ctx context.Context) (string, error) {
	path := p.dbPath()
	if _, err := os.Stat(path); err != nil {
		return "", nil
	}
	uri := (&url.URL{Scheme: "file", Path: path, RawQuery: "mode=ro"}).String()
	_, walErr := os.Stat(path + "-wal")
	_, shmErr := os.Stat(path + "-shm")
	if os.IsNotExist(walErr) && os.IsNotExist(shmErr) {
		uri += "&immutable=1"
	}
	out, err := runCommand(ctx, "sqlite3", uri, sqliteQuery)
	if err != nil {
		slog.Debug("cursor: sqlite3 query failed")
		return "", fmt.Errorf("cursor: could not read Cursor database: %w", err)
	}
	// The PRAGMA prints its own row ("250") first; the token is the last line.
	lines := strings.Split(strings.TrimSpace(string(out)), "\n")
	token := strings.TrimSpace(lines[len(lines)-1])
	if token == "" {
		return "", nil
	}
	if strings.HasPrefix(token, "cursor-safe-storage:") {
		return "", errSafeStorage
	}
	return token, nil
}

// agentToken reads the cursor-agent auth file's accessToken, if present.
func (p *Provider) agentToken() (string, bool) {
	data, err := os.ReadFile(p.authFilePath())
	if err != nil {
		return "", false
	}
	var auth map[string]string
	if json.Unmarshal(data, &auth) != nil {
		return "", false
	}
	token := strings.TrimSpace(auth["accessToken"])
	return token, token != ""
}

// decodeJWT returns the JWT sub claim and, when present, the exp claim. The
// signature is not verified; the server accepts or rejects the cookie.
func decodeJWT(token string) (string, *time.Time, error) {
	parts := strings.Split(token, ".")
	if len(parts) < 2 {
		return "", nil, errors.New("cursor: invalid access token format")
	}
	payload := parts[1]
	if rem := len(payload) % 4; rem > 0 {
		payload += strings.Repeat("=", 4-rem)
	}
	raw, err := base64.URLEncoding.DecodeString(payload)
	if err != nil {
		return "", nil, errors.New("cursor: could not decode access token payload")
	}
	var claims struct {
		Sub string   `json:"sub"`
		Exp *float64 `json:"exp"`
	}
	if json.Unmarshal(raw, &claims) != nil || claims.Sub == "" {
		return "", nil, errors.New("cursor: access token is missing a user id")
	}
	var exp *time.Time
	if claims.Exp != nil {
		t := time.Unix(int64(*claims.Exp), 0)
		exp = &t
	}
	return claims.Sub, exp, nil
}

// cookieUserID is the part of the JWT sub after the last "|", validated
// before it goes into a header.
func cookieUserID(sub string) (string, error) {
	// The id is the part of the sub after the last "|".
	id := sub[strings.LastIndex(sub, "|")+1:]
	if id == "" || strings.Trim(id, "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._-") != "" {
		return "", errors.New("cursor: access token has an invalid user id")
	}
	return id, nil
}

func (p *Provider) fetchOnce(ctx context.Context, token, user string) (monitor.Snapshot, error) {
	ctx, cancel := context.WithTimeout(ctx, requestTimeout)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, p.baseURL+usagePath, nil)
	if err != nil {
		return monitor.Snapshot{}, err
	}
	req.Header.Set("Cookie", "WorkosCursorSessionToken="+user+"%3A%3A"+token)
	req.Header.Set("Content-Type", "application/json")
	slog.Debug("cursor: fetching usage summary")
	resp, err := p.client.Do(req)
	if err != nil {
		return monitor.Snapshot{}, fmt.Errorf("cursor: usage request failed: %w", err)
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(io.LimitReader(resp.Body, maxBodyBytes))
	if err != nil {
		return monitor.Snapshot{}, fmt.Errorf("cursor: could not read usage response: %w", err)
	}
	switch resp.StatusCode {
	case http.StatusOK:
		snap, err := parseUsageSummary(body)
		slog.Debug("cursor: probe success", "quotas", len(snap.Quotas))
		return snap, err
	case http.StatusUnauthorized, http.StatusForbidden:
		return monitor.Snapshot{}, errAuth
	case http.StatusTooManyRequests:
		return monitor.Snapshot{}, &monitor.RateLimitedError{RetryAt: retryAt(resp, time.Now())}
	default:
		return monitor.Snapshot{}, fmt.Errorf("cursor: usage request failed with HTTP %d", resp.StatusCode)
	}
}

func retryAt(resp *http.Response, now time.Time) time.Time {
	v := strings.TrimSpace(resp.Header.Get("Retry-After"))
	if secs, err := strconv.Atoi(v); err == nil && secs >= 0 {
		return now.Add(time.Duration(secs) * time.Second)
	}
	if at, err := http.ParseTime(v); err == nil && at.After(now) {
		return at
	}
	return now.Add(fallbackRetry)
}

type usageMeter struct {
	Enabled          bool               `json:"enabled"`
	Used             float64            `json:"used"`
	Limit            float64            `json:"limit"`
	Breakdown        map[string]float64 `json:"breakdown"`
	TotalPercentUsed *float64           `json:"totalPercentUsed"`
}

type usageSummary struct {
	MembershipType  *string                `json:"membershipType"`
	LimitType       string                 `json:"limitType"`
	IsUnlimited     bool                   `json:"isUnlimited"`
	BillingCycleEnd string                 `json:"billingCycleEnd"`
	IndividualUsage map[string]*usageMeter `json:"individualUsage"`
	TeamUsage       map[string]*usageMeter `json:"teamUsage"`
}

// parseUsageSummary maps the usage-summary response onto quotas, keeping the
// Swift plan-capacity maths: capacity is max(limit, breakdown.total) and
// totalPercentUsed is authoritative when present.
func parseUsageSummary(data []byte) (monitor.Snapshot, error) {
	var summary usageSummary
	if json.Unmarshal(data, &summary) != nil {
		return monitor.Snapshot{}, errors.New("cursor: could not parse usage response")
	}
	membership := "unknown"
	if summary.MembershipType != nil {
		membership = *summary.MembershipType
	}
	var resetsAt *time.Time
	if t, err := time.Parse(time.RFC3339Nano, summary.BillingCycleEnd); err == nil {
		resetsAt = &t
	}
	var quotas []monitor.Quota
	if q, ok := meter(summary.IndividualUsage["plan"], "Monthly", resetsAt); ok {
		quotas = append(quotas, q)
	}
	if q, ok := meter(summary.IndividualUsage["onDemand"], "On-Demand", resetsAt); ok {
		quotas = append(quotas, q)
	}
	if summary.LimitType == "team" {
		if q, ok := meter(summary.TeamUsage["onDemand"], "Team", resetsAt); ok {
			quotas = append(quotas, q)
		}
	}
	if summary.IsUnlimited {
		quotas = append(quotas, monitor.NewPercentQuota(100, monitor.TimeLimitType("Monthly"), "cursor"))
	}
	if len(quotas) == 0 {
		return monitor.Snapshot{}, errors.New("cursor: no usage data in response")
	}
	var tier *monitor.Tier
	if membership != "" {
		t := monitor.CustomTier(strings.ToUpper(membership))
		tier = &t
	}
	return monitor.Snapshot{ProviderID: "cursor", Quotas: quotas, CapturedAt: time.Now(), Tier: tier}, nil
}

// meter builds one enabled usage meter, or false when disabled or without a
// positive limit. used/limit describe only the included allotment; with bonus
// credits the real capacity is breakdown.total, and enterprise plans carry
// everything in the breakdown. Take the larger.
func meter(m *usageMeter, name string, resetsAt *time.Time) (monitor.Quota, bool) {
	if m == nil || !m.Enabled {
		return monitor.Quota{}, false
	}
	used := int(m.Used)
	limit := int(m.Limit)
	if total, ok := m.Breakdown["total"]; ok {
		limit = max(limit, int(total))
	}
	if limit <= 0 {
		return monitor.Quota{}, false
	}
	var remaining float64
	var effectiveUsed int
	if m.TotalPercentUsed != nil {
		remaining = 100 - *m.TotalPercentUsed
		effectiveUsed = int(math.Round(*m.TotalPercentUsed * float64(limit) / 100))
	} else {
		remaining = float64(limit-used) / float64(limit) * 100
		effectiveUsed = used
	}
	q := monitor.NewPercentQuota(max(0, remaining), monitor.TimeLimitType(name), "cursor")
	q.ResetsAt = resetsAt
	q.UnitsUsed = &effectiveUsed
	q.UnitsLimit = &limit
	return q, true
}
