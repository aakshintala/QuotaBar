package opencode

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"strconv"
	"strings"
	"time"

	"quotad/monitor"
)

// Fetch reports the three quota windows through the monitor.
func (p *Provider) Fetch(ctx context.Context) (monitor.Snapshot, error) {
	key, err := p.loadKey(ctx)
	if err != nil {
		return monitor.Snapshot{}, err
	}
	if key == "" {
		slog.Error("opencode: no API key found")
		return monitor.Snapshot{}, loginError()
	}

	reqCtx, cancel := context.WithTimeout(ctx, p.timeout)
	defer cancel()
	req, err := http.NewRequestWithContext(reqCtx, http.MethodGet, p.baseURL, nil)
	if err != nil {
		return monitor.Snapshot{}, fmt.Errorf("opencode: cannot build request")
	}
	req.Header.Set("Authorization", "Bearer "+key)
	req.Header.Set("Accept", "application/json")

	resp, err := p.client.Do(req)
	if err != nil {
		return monitor.Snapshot{}, fmt.Errorf("opencode: request failed")
	}
	defer resp.Body.Close()

	switch resp.StatusCode {
	case http.StatusOK:
	case http.StatusTooManyRequests:
		return monitor.Snapshot{}, p.rateLimited(resp)
	case http.StatusUnauthorized, http.StatusForbidden:
		slog.Error("opencode: API rejected the key", "status", resp.StatusCode)
		return monitor.Snapshot{}, loginError()
	default:
		return monitor.Snapshot{}, fmt.Errorf("opencode: unexpected HTTP status %d", resp.StatusCode)
	}

	body, err := io.ReadAll(io.LimitReader(resp.Body, maxBody))
	if err != nil {
		return monitor.Snapshot{}, fmt.Errorf("opencode: cannot read response")
	}
	quotas, err := parseUsageResponse(body)
	if err != nil {
		return monitor.Snapshot{}, err
	}

	slog.Debug("opencode: probe success")
	return monitor.Snapshot{
		ProviderID: ProviderID,
		Quotas:     quotas,
		CapturedAt: time.Now(),
	}, nil
}

// rateLimited honors Retry-After (seconds, including zero, or an HTTP
// date) and falls back to a default delay only for a missing or invalid
// header.
func (p *Provider) rateLimited(resp *http.Response) *monitor.RateLimitedError {
	delay := defaultRateDelay
	if raw := strings.TrimSpace(resp.Header.Get("Retry-After")); raw != "" {
		if secs, err := strconv.Atoi(raw); err == nil && secs >= 0 {
			delay = time.Duration(secs) * time.Second
		} else if at, err := http.ParseTime(raw); err == nil {
			delay = max(time.Duration(0), time.Until(at))
		}
	}
	return &monitor.RateLimitedError{RetryAt: time.Now().Add(delay)}
}

// apiWindow is one usage window: percent is percent used, already 0-100.
type apiWindow struct {
	Percent  *float64 `json:"percent"`
	ResetsAt *string  `json:"resetsAt"`
}

// parseUsageResponse maps the API response onto the three quotas, as the
// Swift probe does: session, weekly, then a Monthly time limit.
func parseUsageResponse(body []byte) ([]monitor.Quota, error) {
	var response struct {
		Usage struct {
			Rolling apiWindow `json:"rolling"`
			Weekly  apiWindow `json:"weekly"`
			Monthly apiWindow `json:"monthly"`
		} `json:"usage"`
	}
	if err := json.Unmarshal(body, &response); err != nil {
		return nil, fmt.Errorf("opencode: cannot decode usage response")
	}
	windows := []struct {
		window apiWindow
		typ    monitor.QuotaType
	}{
		{response.Usage.Rolling, monitor.SessionType()},
		{response.Usage.Weekly, monitor.WeeklyType()},
		{response.Usage.Monthly, monitor.TimeLimitType("Monthly")},
	}
	quotas := make([]monitor.Quota, 0, len(windows))
	for _, w := range windows {
		if w.window.Percent == nil {
			return nil, fmt.Errorf("opencode: usage response is missing a window")
		}
		q := monitor.NewPercentQuota(percentRemaining(*w.window.Percent), w.typ, ProviderID)
		if reset := parseResetTime(w.window.ResetsAt); reset != nil {
			q.ResetsAt = reset
		}
		quotas = append(quotas, q)
	}
	return quotas, nil
}

// percentRemaining flips percent used to percent remaining, clamped to
// [0, 100]: over-limit usage reports zero, like the Swift probe.
func percentRemaining(used float64) float64 {
	return 100 - min(100, max(0, used))
}

// parseResetTime parses an ISO 8601 reset, accepting fractional and plain
// forms. A missing or unparseable value yields nil, never an error.
func parseResetTime(raw *string) *time.Time {
	if raw == nil || *raw == "" {
		return nil
	}
	if t, err := time.Parse(time.RFC3339Nano, *raw); err == nil {
		return &t
	}
	return nil
}
