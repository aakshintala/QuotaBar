package feed

import (
	"context"
	"encoding/json"
	"strings"
	"testing"
	"time"

	"quotad/monitor"
)

func TestSessionStartRendersContext(t *testing.T) {
	captured := time.Unix(1_700_000_000, 0)
	now := captured.Add(30 * time.Second)
	plus := monitor.CustomTier("Plus")
	m := monitor.New([]monitor.Provider{
		&staticProvider{id: "codex", snapshot: monitor.Snapshot{
			ProviderID: "codex",
			Quotas:     []monitor.Quota{monitor.NewPercentQuota(81, monitor.WeeklyType(), "codex")},
			CapturedAt: captured, Tier: &plus,
		}},
	},
		monitor.WithClock(fixedClock{now}),
		monitor.WithDisplayName(func(string) string { return "Codex" }))
	m.Refresh(context.Background(), true)
	hooks := NewHooks(func() Feed { return Build(m, now) }, func() time.Time { return now })
	got := hooks.SessionStart([]byte(`{"session_id":"s1","hook_event_name":"SessionStart"}`))
	var envelope map[string]any
	if err := json.Unmarshal(got, &envelope); err != nil {
		t.Fatalf("session-start is not JSON: %v", err)
	}
	output := envelope["hookSpecificOutput"].(map[string]any)
	if output["hookEventName"] != "SessionStart" {
		t.Errorf("event = %v", output["hookEventName"])
	}
	want := "Quota headroom (QuotaBar):\ncodex (Plus) - weekly 81%"
	if output["additionalContext"] != want {
		t.Errorf("additionalContext = %q, want %q", output["additionalContext"], want)
	}
}

func TestPromptAlertsOnlyOnWorsening(t *testing.T) {
	now := time.Now()
	feedOf := func(status string) Feed {
		return Feed{
			GeneratedAt: ISOTime(now),
			Providers: []ProviderDTO{{
				ID: "codex", Name: "Codex", Status: status,
				CapturedAt: isoTime(now), Quotas: []QuotaDTO{{
					Key: "weekly", Label: "Weekly",
					PercentRemaining: f64(30), Status: status,
				}},
			}},
			DisabledProviderIDs: []string{},
		}
	}
	current := feedOf("healthy")
	hooks := NewHooks(func() Feed { return current }, func() time.Time { return now })
	body := []byte(`{"session_id":"s1"}`)
	hooks.SessionStart(body) // baseline: healthy
	if got := hooks.Prompt(body); string(got) != "{}" {
		t.Errorf("no change should stay silent, got %s", got)
	}
	current = feedOf("warning")
	got := hooks.Prompt(body)
	var envelope map[string]any
	if err := json.Unmarshal(got, &envelope); err != nil {
		t.Fatalf("prompt alert is not JSON: %v", err)
	}
	output := envelope["hookSpecificOutput"].(map[string]any)
	if output["hookEventName"] != "UserPromptSubmit" {
		t.Errorf("event = %v", output["hookEventName"])
	}
	if context, ok := output["additionalContext"].(string); !ok ||
		!strings.HasPrefix(context, "Quota alert (QuotaBar):\n") ||
		!strings.Contains(context, "codex") {
		t.Errorf("additionalContext = %q", output["additionalContext"])
	}
	// Repeating the same state stays silent: it was already reported.
	if got := hooks.Prompt(body); string(got) != "{}" {
		t.Errorf("repeated state should stay silent, got %s", got)
	}
}

func TestPromptImprovementStaysSilent(t *testing.T) {
	now := time.Now()
	feedOf := func(status string) Feed {
		return Feed{
			GeneratedAt: ISOTime(now),
			Providers: []ProviderDTO{{
				ID: "codex", Name: "Codex", Status: status,
				CapturedAt: isoTime(now), Quotas: []QuotaDTO{{
					Key: "weekly", Label: "Weekly",
					PercentRemaining: f64(30), Status: status,
				}},
			}},
			DisabledProviderIDs: []string{},
		}
	}
	current := feedOf("warning")
	hooks := NewHooks(func() Feed { return current }, func() time.Time { return now })
	body := []byte(`{"session_id":"s9"}`)
	hooks.SessionStart(body) // baseline records warning
	current = feedOf("healthy")
	if got := hooks.Prompt(body); string(got) != "{}" {
		t.Errorf("improvement should stay silent, got %s", got)
	}
}
