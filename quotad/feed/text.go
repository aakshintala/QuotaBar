package feed

import (
	"fmt"
	"math"
	"quotad/monitor"
	"strings"
	"time"
)

func Render(f Feed, now time.Time) string {
	var groups []string
	for _, p := range f.Providers {
		groups = append(groups, strings.Join(RenderProvider(p, now), "\n"))
	}
	lines := strings.Join(groups, "\n\n")
	if len(f.DisabledProviderIDs) > 0 {
		lines += "\n\n(disabled in QuotaBar: " + strings.Join(f.DisabledProviderIDs, ", ") + ")"
	}
	return strings.Trim(lines, "\n")
}
func RenderProvider(p ProviderDTO, now time.Time) []string {
	tier := ""
	if p.Tier != nil {
		tier = " (" + *p.Tier + ")"
	}
	if p.ThrottledUntil != nil {
		return []string{fmt.Sprintf("%s%s - throttled until %s, showing last known data (%s)",
			p.ID, tier, formatClock(time.Time(*p.ThrottledUntil)), formatAge(p.AgeSeconds))}
	}
	if p.Unavailable != nil {
		return []string{fmt.Sprintf("%s%s - unavailable: %s", p.ID, tier, *p.Unavailable)}
	}
	if p.CapturedAt == nil {
		return []string{fmt.Sprintf("%s%s - no data yet", p.ID, tier)}
	}
	if p.ThrottledUntil == nil && !anyDetail(p.Quotas) {
		var parts []string
		for _, q := range p.Quotas {
			part := strings.ToLower(q.Label) + " " + amount(q)
			if extra, ok := quotaExtra(q); ok {
				part += " (" + extra + ")"
			}
			parts = append(parts, part)
		}
		summary := strings.Join(parts, " · ")
		if summary == "" {
			summary = "no quotas reported"
		}
		return []string{fmt.Sprintf("%s%s - %s", p.ID, tier, summary)}
	}
	var lines []string
	if p.ThrottledUntil == nil {
		lines = append(lines, fmt.Sprintf("%s%s - data %s", p.ID, tier, formatAge(p.AgeSeconds)))
	}
	for _, q := range p.Quotas {
		note := ""
		if extra, ok := quotaExtra(q); ok {
			note = " (" + extra + ")"
		}
		left := ""
		if q.PercentRemaining != nil {
			left = " left"
		}
		line := fmt.Sprintf("  %s %s%s%s", padLabel(q.Label), amount(q), left, note)
		if !needsDetail(q) {
			lines = append(lines, line)
			continue
		}
		reset := ""
		if q.ResetsAt != nil {
			reset = ", resets in " + formatUntil(time.Time(*q.ResetsAt), now)
		}
		lines = append(lines, line+reset+"  ["+q.Status+"]")
	}
	return lines
}
func anyDetail(quotas []QuotaDTO) bool {
	for _, q := range quotas {
		if needsDetail(q) {
			return true
		}
	}
	return false
}
func needsDetail(q QuotaDTO) bool { return q.Status != "healthy" }
func amount(q QuotaDTO) string {
	if q.PercentRemaining != nil {
		return fmt.Sprintf("%d%%", int(math.Round(*q.PercentRemaining)))
	}
	if left := balanceLeft(q); left != "" {
		return left
	}
	return "?"
}
func quotaExtra(q QuotaDTO) (string, bool) {
	if q.UnitsUsed != nil && q.UnitsLimit != nil && *q.UnitsLimit > 0 {
		return fmt.Sprintf("%d/%d", *q.UnitsUsed, *q.UnitsLimit), true
	}
	if q.PercentRemaining != nil {
		if left := balanceLeft(q); left != "" {
			return left, true
		}
	}
	return "", false
}
func balanceLeft(q QuotaDTO) string {
	if q.BalanceRemaining == nil {
		return ""
	}
	unit := monitor.USD
	if q.BalanceUnit != nil {
		unit = monitor.BalanceUnit(*q.BalanceUnit)
	}
	return unit.Format(*q.BalanceRemaining) + " left"
}
func padLabel(label string) string {
	const width = 8
	if len(label) >= width {
		return label
	}
	return label + strings.Repeat(" ", width-len(label))
}
func formatAge(seconds *int) string {
	if seconds == nil {
		return "no data yet"
	}
	s := *seconds
	switch {
	case s < 60:
		return "just now"
	case s < 3600:
		return fmt.Sprintf("%dm old", s/60)
	case s < 86400:
		h, m := s/3600, (s%3600)/60
		if m > 0 {
			return fmt.Sprintf("%dh%dm old", h, m)
		}
		return fmt.Sprintf("%dh old", h)
	default:
		d, h := s/86400, (s%86400)/3600
		if h > 0 {
			return fmt.Sprintf("%dd%dh old", d, h)
		}
		return fmt.Sprintf("%dd old", d)
	}
}
func formatUntil(date, now time.Time) string {
	totalMinutes := int(date.Sub(now).Minutes())
	days, hours, minutes := totalMinutes/(24*60), (totalMinutes%(24*60))/60, totalMinutes%60
	switch {
	case days > 0:
		if hours > 0 {
			return fmt.Sprintf("%dd%dh", days, hours)
		}
		return fmt.Sprintf("%dd", days)
	case hours > 0:
		if minutes > 0 {
			return fmt.Sprintf("%dh%dm", hours, minutes)
		}
		return fmt.Sprintf("%dh", hours)
	case minutes > 0:
		return fmt.Sprintf("%dm", minutes)
	default:
		return "soon"
	}
}
func formatClock(t time.Time) string { return t.Local().Format("3:04 PM") }
