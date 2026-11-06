// Quota status and value types.
package monitor

import (
	"fmt"
	"math"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"
)

// Status is the pace-aware health of one quota bucket.
type Status int

const (
	// Healthy needs no attention.
	Healthy Status = iota
	// Warning is below half or burning fast.
	Warning
	// Critical is nearly out or about to be.
	Critical
	// Depleted is at or below zero.
	Depleted
)

// NeedsAttention reports whether the status should surface to the user.
func (s Status) NeedsAttention() bool { return s != Healthy }

var statusNames = []string{"healthy", "warning", "critical", "depleted"}

// Key returns the feed and hook status string.
func (s Status) Key() string { return statusNames[s] }

// StatusFromKey parses a status string back into a Status.
func StatusFromKey(key string) (Status, bool) {
	for s, name := range statusNames {
		if name == key {
			return Status(s), true
		}
	}

	return Healthy, false
}

// FromPercent maps remaining percent to a status without pace data.
func FromPercent(remaining float64) Status {
	switch {
	case remaining <= 0:
		return Depleted
	case remaining < 20:
		return Critical
	case remaining < 50:
		return Warning
	default:
		return Healthy
	}
}

const burnRateThreshold = 1.5

// FromPace maps remaining percent and window elapsed percent to a status.
// A slow burn on a low bucket stays healthy; nil or non-positive elapsed
// falls back to FromPercent.
func FromPace(remaining float64, elapsed *float64) Status {
	if remaining <= 0 {
		return Depleted
	}
	if elapsed == nil || *elapsed <= 0 {
		return FromPercent(remaining)
	}

	burn := (100 - remaining) / *elapsed

	if remaining < 20 {
		if burn > 1 {
			return Critical
		}
		return Healthy
	}
	if burn > burnRateThreshold && remaining < 50 {
		return Warning
	}

	return Healthy
}

// QuotaKind names the bucket shape.
type QuotaKind int

const (
	// KindSession is the short rolling window (about five hours).
	KindSession QuotaKind = iota
	// KindWeekly is the seven-day window.
	KindWeekly
	// KindModel is a per-model bucket.
	KindModel
	// KindTime is a named time bucket such as Monthly.
	KindTime
)

// QuotaType is a bucket shape plus its model or time-limit name.
type QuotaType struct {
	Kind QuotaKind
	// Name is the model or time-limit name for KindModel/KindTime.
	Name string
}

// SessionType returns the session-window quota type.
func SessionType() QuotaType { return QuotaType{Kind: KindSession} }

// WeeklyType returns the weekly-window quota type.
func WeeklyType() QuotaType { return QuotaType{Kind: KindWeekly} }

// ModelType returns the per-model quota type for model.
func ModelType(model string) QuotaType { return QuotaType{Kind: KindModel, Name: model} }

// TimeLimitType returns the named time-bucket quota type.
func TimeLimitType(name string) QuotaType { return QuotaType{Kind: KindTime, Name: name} }

// Key returns the feed quota key, such as "session" or "model:fable".
func (t QuotaType) Key() string {
	switch t.Kind {
	case KindSession:
		return "session"
	case KindWeekly:
		return "weekly"
	case KindModel:
		return "model:" + t.Name
	default:
		return "time:" + t.Name
	}
}

// DisplayName returns the label shown beside a quota row.
func (t QuotaType) DisplayName() string {
	switch t.Kind {
	case KindSession:
		return "Session"
	case KindWeekly:
		return "Weekly"
	case KindModel:
		return capitalize(t.Name)
	default:
		return t.Name
	}
}

// Duration returns the window length used for pace maths.
func (t QuotaType) Duration() time.Duration {
	switch {
	case t.Kind == KindSession:
		return 5 * time.Hour
	case t.Kind == KindTime && strings.EqualFold(t.Name, "Monthly"):
		return 30 * 24 * time.Hour
	default:
		return 7 * 24 * time.Hour
	}
}

func capitalize(s string) string {
	lowered := strings.ToLower(s)
	r, size := utf8.DecodeRuneInString(lowered)
	if size == 0 {
		return s
	}

	return string(unicode.ToUpper(r)) + lowered[size:]
}

// BalanceUnit is the currency of a balance meter.
type BalanceUnit string

const (
	// USD renders as dollars and cents.
	USD BalanceUnit = "usd"
	// Credits renders as whole or fractional credits.
	Credits BalanceUnit = "credits"
)

func groupDigits(s string) string {
	neg := strings.HasPrefix(s, "-")
	s = strings.TrimPrefix(s, "-")

	var b strings.Builder
	if neg {
		b.WriteByte('-')
	}

	for i, n := 0, len(s); i < n; i++ {
		if i > 0 && (n-i)%3 == 0 {
			b.WriteByte(',')
		}
		b.WriteByte(s[i])
	}

	return b.String()
}

// Format renders a balance: "$1,234.50" for USD, "1,234 credits" for credits.
func (u BalanceUnit) Format(amount float64) string {
	if u == Credits {
		s := strings.TrimRight(strings.TrimRight(fmt.Sprintf("%.2f", amount), "0"), ".")
		if i := strings.IndexByte(s, '.'); i >= 0 {
			return groupDigits(s[:i]) + s[i:] + " credits"
		}

		return groupDigits(s) + " credits"
	}

	cents := int64(math.Round(amount * 100))
	sign := ""
	if cents < 0 {
		sign, cents = "-", -cents
	}

	return fmt.Sprintf("$%s%s.%02d", sign, groupDigits(fmt.Sprint(cents/100)), cents%100)
}

// Tier is the plan badge shown beside a provider.
type Tier struct {
	Claude bool // well-known Claude tier (Max/Pro/API badge)
	// Badge is the badge text, or a custom badge for other providers.
	Badge string
}

// ClaudeMaxTier returns the Claude Max plan tier, shown as Ultra.
func ClaudeMaxTier() Tier { return Tier{Claude: true, Badge: "ULTRA"} }

// ClaudeProTier returns the Claude Pro tier.
func ClaudeProTier() Tier { return Tier{Claude: true, Badge: "PRO"} }

// ClaudeAPITier returns the Claude API tier.
func ClaudeAPITier() Tier { return Tier{Claude: true, Badge: "API"} }

// CustomTier returns a non-Claude badge tier.
func CustomTier(badge string) Tier { return Tier{Badge: badge} }

// FeedName returns the badge text as shown in the feed.
func (t Tier) FeedName() string {
	if t.Claude {
		return capitalize(t.Badge)
	}

	return t.Badge
}

// Quota is one meter: a percent, a balance, a unit count, or a mix.
type Quota struct {
	PercentRemaining *float64
	Type             QuotaType
	ProviderID       string
	ResetsAt         *time.Time
	BalanceRemaining *float64
	BalanceUsed      *float64
	BalanceCap       *float64
	BalanceUnit      *BalanceUnit
	UnitsUsed        *int
	UnitsLimit       *int
}

// NewPercentQuota builds a percent meter, capped at 100.
func NewPercentQuota(remaining float64, typ QuotaType, providerID string) Quota {
	capped := min(100, remaining)
	return Quota{PercentRemaining: &capped, Type: typ, ProviderID: providerID}
}

// IsBalanceOnly reports whether the quota has no percent meter.
func (q Quota) IsBalanceOnly() bool { return q.PercentRemaining == nil }

// StatusAt returns the pace-aware status at now.
func (q Quota) StatusAt(now time.Time) Status {
	if q.PercentRemaining == nil {
		return Healthy
	}

	return FromPace(*q.PercentRemaining, q.percentElapsedAt(now))
}

func (q Quota) percentElapsedAt(now time.Time) *float64 {
	if q.ResetsAt == nil {
		return nil
	}

	total := q.Type.Duration().Seconds()
	if total <= 0 {
		return nil
	}

	elapsed := min(100, max(0, (total-max(0, q.ResetsAt.Sub(now).Seconds()))/total*100))

	return &elapsed
}

// CompactResetAt renders the reset countdown, such as "2d" or "soon".
func (q Quota) CompactResetAt(now time.Time) string {
	if q.ResetsAt == nil {
		return ""
	}

	switch s := int(q.ResetsAt.Sub(now).Seconds()); {
	case s >= 86400:
		return fmt.Sprintf("%dd", s/86400)
	case s >= 3600:
		return fmt.Sprintf("%dh", s/3600)
	case s >= 60:
		return fmt.Sprintf("%dm", s/60)
	default:
		return "soon"
	}
}

// Snapshot is one provider's quotas captured at one moment.
type Snapshot struct {
	ProviderID string
	Quotas     []Quota
	CapturedAt time.Time
	Tier       *Tier
}

// OverallStatusAt returns the worst percent-meter status at now.
func (s Snapshot) OverallStatusAt(now time.Time) Status {
	worst := Healthy

	for _, q := range s.Quotas {
		if !q.IsBalanceOnly() {
			worst = max(worst, q.StatusAt(now))
		}
	}

	return worst
}
