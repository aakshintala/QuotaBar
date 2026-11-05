package monitor

import (
	"fmt"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"
)

type Status int

const (
	Healthy Status = iota
	Warning
	Critical
	Depleted
)

func (s Status) NeedsAttention() bool { return s != Healthy }

var statusNames = []string{"healthy", "warning", "critical", "depleted"}

func (s Status) Key() string { return statusNames[s] }
func StatusFromKey(key string) (Status, bool) {
	for s, name := range statusNames {
		if name == key {
			return Status(s), true
		}
	}
	return Healthy, false
}
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

type QuotaKind int

const (
	KindSession QuotaKind = iota
	KindWeekly
	KindModel
	KindTime
)

type QuotaType struct {
	Kind QuotaKind
	Name string // model name or time-limit name for KindModel/KindTime
}

func SessionType() QuotaType              { return QuotaType{Kind: KindSession} }
func WeeklyType() QuotaType               { return QuotaType{Kind: KindWeekly} }
func ModelType(model string) QuotaType    { return QuotaType{Kind: KindModel, Name: model} }
func TimeLimitType(name string) QuotaType { return QuotaType{Kind: KindTime, Name: name} }
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

type BalanceUnit string

const (
	USD     BalanceUnit = "usd"
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
func (u BalanceUnit) Format(amount float64) string {
	if u == Credits {
		s := strings.TrimRight(strings.TrimRight(fmt.Sprintf("%.2f", amount), "0"), ".")
		if i := strings.IndexByte(s, '.'); i >= 0 {
			return groupDigits(s[:i]) + s[i:] + " credits"
		}
		return groupDigits(s) + " credits"
	}
	whole := int64(amount)
	frac := int64((amount - float64(whole)) * 100)
	if frac < 0 {
		frac = 0
	}
	return fmt.Sprintf("$%s.%02d", groupDigits(fmt.Sprint(whole)), frac)
}

type Tier struct {
	Claude bool   // well-known Claude tier (Max/Pro/API badge)
	Badge  string // badge text, or a custom badge for other providers
}

func ClaudeMaxTier() Tier          { return Tier{Claude: true, Badge: "MAX"} }
func ClaudeProTier() Tier          { return Tier{Claude: true, Badge: "PRO"} }
func ClaudeAPITier() Tier          { return Tier{Claude: true, Badge: "API"} }
func CustomTier(badge string) Tier { return Tier{Badge: badge} }
func (t Tier) FeedName() string {
	if t.Claude {
		return capitalize(t.Badge)
	}
	return t.Badge
}

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

func NewPercentQuota(remaining float64, typ QuotaType, providerID string) Quota {
	capped := min(100, remaining)
	return Quota{PercentRemaining: &capped, Type: typ, ProviderID: providerID}
}
func (q Quota) IsBalanceOnly() bool { return q.PercentRemaining == nil }
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

type Snapshot struct {
	ProviderID string
	Quotas     []Quota
	CapturedAt time.Time
	Tier       *Tier
}

func (s Snapshot) OverallStatusAt(now time.Time) Status {
	worst := Healthy
	for _, q := range s.Quotas {
		if !q.IsBalanceOnly() {
			worst = max(worst, q.StatusAt(now))
		}
	}
	return worst
}
