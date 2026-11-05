// Package opencode is the OpenCode Go quota provider (id "opencode-go").
//
// It reads the OpenCode CLI's own stored API key — first the credential
// table in opencode.db (or $OPENCODE_DB) through the sqlite3 CLI, then
// auth.json — and reports the three server-accounted windows (5h, weekly,
// monthly) from opencode's quota API. Read-only: no refresh, no writeback,
// and no local cost estimate (the estimate undercounted server deductions
// and was removed on purpose).
package opencode

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"math"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
	"unicode"
)

// ProviderID is the settings key and feed id. Never change a shipped id.
const ProviderID = "opencode-go"

// DefaultBaseURL is opencode's quota API. Tests override it.
const DefaultBaseURL = "https://opencode.ai/zen/go/v1/usage"

const (
	defaultTimeout   = 15 * time.Second
	sqliteTimeout    = 10 * time.Second
	maxBody          = 1 << 20
	defaultRateDelay = 5 * time.Minute
)

// Provider fetches OpenCode Go quotas.
type Provider struct {
	home      string
	lookupEnv func(string) (string, bool)
	baseURL   string
	client    *http.Client
	timeout   time.Duration
}

// Option customizes a Provider.
type Option func(*Provider)

// WithHome sets the home directory (tests point it at a temp dir).
func WithHome(home string) Option { return func(p *Provider) { p.home = home } }

// WithLookupEnv sets environment lookup (tests avoid the real environment).
func WithLookupEnv(lookup func(string) (string, bool)) Option {
	return func(p *Provider) { p.lookupEnv = lookup }
}

// WithBaseURL overrides the quota API URL (tests use httptest).
func WithBaseURL(url string) Option { return func(p *Provider) { p.baseURL = url } }

// WithClient sets the HTTP client.
func WithClient(client *http.Client) Option { return func(p *Provider) { p.client = client } }

// WithTimeout sets the per-request timeout.
func WithTimeout(d time.Duration) Option { return func(p *Provider) { p.timeout = d } }

// New builds a Provider with sensible defaults.
func New(opts ...Option) *Provider {
	home, _ := os.UserHomeDir()
	p := &Provider{
		home:      home,
		lookupEnv: os.LookupEnv,
		baseURL:   DefaultBaseURL,
		client:    &http.Client{},
		timeout:   defaultTimeout,
	}
	for _, opt := range opts {
		opt(p)
	}
	return p
}

// ID returns the provider id.
func (p *Provider) ID() string { return ProviderID }

// Available is cheap: it only stats for a credential source, with no
// network and no process calls.
func (p *Provider) Available() bool {
	if db := p.dbPath(); db != "" {
		if _, err := os.Stat(db); err == nil {
			return true
		}
	}
	if _, err := os.Stat(p.authFilePath()); err == nil {
		return true
	}
	return false
}

// dataDir is $XDG_DATA_HOME/opencode when XDG_DATA_HOME is absolute,
// else ~/.local/share/opencode. Same on both OSes.
func (p *Provider) dataDir() string {
	if xdg, ok := p.lookupEnv("XDG_DATA_HOME"); ok && strings.HasPrefix(xdg, "/") {
		return filepath.Join(xdg, "opencode")
	}
	return filepath.Join(p.home, ".local", "share", "opencode")
}

// authFilePath is opencode's auth file inside the data dir.
func (p *Provider) authFilePath() string {
	return filepath.Join(p.dataDir(), "auth.json")
}

// dbPath is the credential database: $OPENCODE_DB when set to a usable
// value, else opencode.db in the data dir. Empty means no database source.
func (p *Provider) dbPath() string {
	name, ok := p.lookupEnv("OPENCODE_DB")
	if !ok || name == "" {
		name = "opencode.db"
	}
	if name == ":memory:" {
		return ""
	}
	if filepath.IsAbs(name) {
		return name
	}
	return filepath.Join(p.dataDir(), name)
}

// runSQLite executes the sqlite3 CLI and returns its stdout. It is a
// variable so tests can stub the process without touching real credentials.
var runSQLite = func(ctx context.Context, dbPath, sql string) ([]byte, []byte, error) {
	ctx, cancel := context.WithTimeout(ctx, sqliteTimeout)
	defer cancel()
	cmd := exec.CommandContext(ctx, "sqlite3",
		"-batch", "-readonly", "-noheader", "-separator", "\x1f", dbPath, sql)
	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	err := cmd.Run()
	return stdout.Bytes(), stderr.Bytes(), err
}

// credentialQuery reads the active key for opencode-go first, then opencode.
const credentialQuery = `SELECT integration_id, value FROM credential` +
	` WHERE active = 1 AND integration_id IN ('opencode-go', 'opencode')` +
	` ORDER BY CASE integration_id WHEN 'opencode-go' THEN 0 ELSE 1 END LIMIT 2`

// dbCredential holds the decoded JSON in the credential.value column.
type dbCredential struct {
	Type    string   `json:"type"`
	Key     string   `json:"key"`
	Access  string   `json:"access"`
	Expires *float64 `json:"expires"`
}

// loadKey returns the API key: the database credential first, then the
// auth.json entry. A nil key with a nil error means no credential found.
func (p *Provider) loadKey(ctx context.Context) (string, error) {
	if db := p.dbPath(); db != "" {
		if _, err := os.Stat(db); err == nil {
			key, fallback, err := p.loadDBKey(ctx, db)
			if err != nil {
				return "", err
			}
			if !fallback && key != "" {
				return key, nil
			}
			if !fallback {
				return "", nil
			}
		}
	}
	return p.loadFileKey()
}

// loadDBKey reads the key from the credential table. The fallback result
// is true when the database or the table is absent, meaning the caller
// should try auth.json instead.
func (p *Provider) loadDBKey(ctx context.Context, db string) (string, bool, error) {
	stdout, stderr, err := runSQLite(ctx, db, credentialQuery)
	if err != nil {
		if bytes.Contains(stderr, []byte("no such table")) {
			return "", true, nil
		}
		slog.Error("opencode: cannot read credential database")
		return "", false, loginError()
	}
	rows := parseRows(string(stdout))
	if len(rows) == 0 {
		return "", false, nil
	}
	if len(rows) == 2 && rows[0].integration == rows[1].integration {
		slog.Error("opencode: ambiguous credential rows")
		return "", false, loginError()
	}
	var cred dbCredential
	if err := json.Unmarshal([]byte(rows[0].value), &cred); err != nil {
		slog.Error("opencode: cannot decode credential row")
		return "", false, loginError()
	}
	switch cred.Type {
	case "key":
		return validToken(cred.Key)
	case "oauth":
		if cred.Expires == nil || math.IsNaN(*cred.Expires) || math.IsInf(*cred.Expires, 0) {
			return "", false, loginError()
		}
		if *cred.Expires < 0 || *cred.Expires != math.Trunc(*cred.Expires) {
			return "", false, loginError()
		}
		if *cred.Expires/1000 <= float64(time.Now().Unix()) {
			slog.Error("opencode: stored credential is expired")
			return "", false, loginError()
		}
		return validToken(cred.Access)
	default:
		slog.Error("opencode: stored credential has unknown type")
		return "", false, loginError()
	}
}

// dbRow is one integration_id, value pair from the credential query.
type dbRow struct {
	integration string
	value       string
}

// parseRows splits sqlite3 separator-delimited output into rows.
func parseRows(out string) []dbRow {
	var rows []dbRow
	for _, line := range strings.Split(out, "\n") {
		line = strings.TrimSuffix(line, "\r")
		if line == "" {
			continue
		}
		integration, value, _ := strings.Cut(line, "\x1f")
		rows = append(rows, dbRow{integration: integration, value: value})
	}
	return rows
}

// loadFileKey reads opencode-go.key from auth.json, like the Swift loader:
// a non-empty key wins, whatever its type.
func (p *Provider) loadFileKey() (string, error) {
	data, err := os.ReadFile(p.authFilePath())
	if err != nil {
		return "", nil
	}
	var file map[string]struct {
		Key string `json:"key"`
	}
	if err := json.Unmarshal(data, &file); err != nil {
		slog.Error("opencode: cannot decode auth file")
		return "", nil
	}
	entry, ok := file["opencode-go"]
	if !ok || entry.Key == "" {
		return "", nil
	}
	return entry.Key, nil
}

// validToken rejects an empty token or one carrying whitespace or control
// characters, without ever logging the value.
func validToken(token string) (string, bool, error) {
	if token == "" {
		return "", false, loginError()
	}
	for _, r := range token {
		if unicode.IsSpace(r) || unicode.IsControl(r) {
			slog.Error("opencode: stored credential is malformed")
			return "", false, loginError()
		}
	}
	return token, false, nil
}

// loginError tells the user to log in to the opencode CLI again. It never
// carries a token, a path, or database contents.
func loginError() error {
	return fmt.Errorf("opencode: no valid API key; log in to the opencode CLI again")
}
