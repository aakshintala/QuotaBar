// Package feed serves the quota state over localhost HTTP: the QuotaFeedDTO
// JSON, the Claude Code hook endpoints, and their text rendering.
package feed

import (
	"encoding/json"
	"errors"
	"quotad/monitor"
	"time"
)

// ISOTime marshals as RFC 3339, matching the Swift feed dates.
type ISOTime time.Time

// MarshalJSON encodes the time as a quoted RFC 3339 string.
func (t ISOTime) MarshalJSON() ([]byte, error) {
	return []byte(`"` + time.Time(t).UTC().Format(time.RFC3339) + `"`), nil
}

// UnmarshalJSON decodes a quoted RFC 3339 string.
func (t *ISOTime) UnmarshalJSON(data []byte) error {
	var s string
	if err := json.Unmarshal(data, &s); err != nil {
		return err
	}

	parsed, err := time.Parse(time.RFC3339, s)
	if err != nil {
		return err
	}

	*t = ISOTime(parsed)

	return nil
}

func isoTime(t time.Time) *ISOTime {
	v := ISOTime(t)
	return &v
}

// Feed is the whole /quotas document.
type Feed struct {
	GeneratedAt         ISOTime       `json:"generatedAt"`
	Providers           []ProviderDTO `json:"providers"`
	DisabledProviderIDs []string      `json:"disabledProviderIds"`
}

// ProviderDTO is one provider's feed entry.
type ProviderDTO struct {
	ID             string     `json:"id"`
	Name           string     `json:"name"`
	Tier           *string    `json:"tier,omitempty"`
	CapturedAt     *ISOTime   `json:"capturedAt,omitempty"`
	AgeSeconds     *int       `json:"ageSeconds,omitempty"`
	Status         string     `json:"status"`
	Unavailable    *string    `json:"unavailable,omitempty"`
	ThrottledUntil *ISOTime   `json:"throttledUntil,omitempty"`
	Quotas         []QuotaDTO `json:"quotas"`
}

// QuotaDTO is one quota bucket's feed entry.
type QuotaDTO struct {
	Key              string   `json:"key"`
	Label            string   `json:"label"`
	PercentRemaining *float64 `json:"percentRemaining,omitempty"`
	ResetsAt         *ISOTime `json:"resetsAt,omitempty"`
	ResetText        *string  `json:"resetText,omitempty"`
	Status           string   `json:"status"`
	BalanceRemaining *float64 `json:"balanceRemaining,omitempty"`
	BalanceUsed      *float64 `json:"balanceUsed,omitempty"`
	BalanceCap       *float64 `json:"balanceCap,omitempty"`
	BalanceUnit      *string  `json:"balanceUnit,omitempty"`
	UnitsUsed        *int     `json:"unitsUsed,omitempty"`
	UnitsLimit       *int     `json:"unitsLimit,omitempty"`
}

// Build snapshots the monitor into a feed document at now.
func Build(m *monitor.Monitor, now time.Time) Feed {
	feed := Feed{GeneratedAt: ISOTime(now), Providers: []ProviderDTO{}, DisabledProviderIDs: []string{}}

	for _, p := range m.Providers() {
		if !m.Enabled(p.ID()) {
			feed.DisabledProviderIDs = append(feed.DisabledProviderIDs, p.ID())
			continue
		}

		feed.Providers = append(feed.Providers, buildProvider(m, p.ID(), now))
	}

	return feed
}

func buildProvider(m *monitor.Monitor, id string, now time.Time) ProviderDTO {
	out := ProviderDTO{ID: id, Name: m.DisplayName(id), Status: "unknown", Quotas: []QuotaDTO{}}

	snap, ok := m.SnapshotOf(id)
	if ok {
		out.CapturedAt = isoTime(snap.CapturedAt)

		age := int(now.Sub(snap.CapturedAt).Seconds())
		out.AgeSeconds = &age

		if snap.Tier != nil {
			name := snap.Tier.FeedName()
			out.Tier = &name
		}

		for _, q := range snap.Quotas {
			out.Quotas = append(out.Quotas, buildQuota(q, now))
		}

		if len(snap.Quotas) > 0 {
			out.Status = snap.OverallStatusAt(now).Key()
		}
	}

	if err := m.LastError(id); err != nil {
		var throttled *monitor.RateLimitedError
		if errors.As(err, &throttled) {
			out.ThrottledUntil = isoTime(throttled.RetryAt)
		} else {
			msg := err.Error()
			out.Unavailable = &msg
		}
	} else if ok && len(snap.Quotas) == 0 {
		msg := "No current quota data"
		out.Unavailable = &msg
	}

	return out
}

func buildQuota(q monitor.Quota, now time.Time) QuotaDTO {
	out := QuotaDTO{
		Key: q.Type.Key(), Label: q.Type.DisplayName(), PercentRemaining: q.PercentRemaining,
		Status: q.StatusAt(now).Key(), BalanceRemaining: q.BalanceRemaining,
		BalanceUsed: q.BalanceUsed, BalanceCap: q.BalanceCap,
		UnitsUsed: q.UnitsUsed, UnitsLimit: q.UnitsLimit,
	}

	if q.ResetsAt != nil {
		out.ResetsAt = isoTime(*q.ResetsAt)
	}

	if compact := q.CompactResetAt(now); compact != "" {
		text := "Resets in " + compact
		if compact == "soon" {
			text = "Resets soon"
		}
		out.ResetText = &text
	}

	if q.BalanceUnit != nil {
		unit := string(*q.BalanceUnit)
		out.BalanceUnit = &unit
	}

	return out
}
