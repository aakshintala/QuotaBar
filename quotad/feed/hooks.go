// Claude Code hook handlers for package feed.
package feed

import (
	"encoding/json"
	"quotad/monitor"
	"strings"
	"sync"
	"time"
)

// Hooks serves the SessionStart and UserPromptSubmit hook endpoints. Hooks
// never trigger a probe; they only read the cached feed.
type Hooks struct {
	mu       sync.Mutex
	feed     func() Feed
	now      func() time.Time
	reported map[string]map[string]monitor.Status
}

// NewHooks builds Hooks over a feed function and a clock.
func NewHooks(feed func() Feed, now func() time.Time) *Hooks {
	if now == nil {
		now = time.Now
	}

	return &Hooks{feed: feed, now: now, reported: map[string]map[string]monitor.Status{}}
}

var emptyJSON = []byte("{}")

type hookEnvelope struct {
	HookSpecificOutput hookOutput `json:"hookSpecificOutput"`
}

type hookOutput struct {
	HookEventName     string `json:"hookEventName"`
	AdditionalContext string `json:"additionalContext"`
}

func envelope(event, context string) []byte {
	data, err := json.Marshal(hookEnvelope{hookOutput{event, context}})
	if err != nil {
		return emptyJSON
	}

	return data
}

func statusesIn(f Feed) map[string]monitor.Status {
	result := map[string]monitor.Status{}

	for _, p := range f.Providers {
		for _, q := range p.Quotas {
			if st, ok := monitor.StatusFromKey(q.Status); ok {
				result[p.ID+"/"+q.Key] = st
			}
		}
	}

	return result
}

func sessionID(body []byte) string {
	var v struct {
		SessionID string `json:"session_id"`
	}
	if json.Unmarshal(body, &v) != nil {
		return ""
	}

	return v.SessionID
}

// SessionStart returns the cached feed as hook context text.
func (h *Hooks) SessionStart(body []byte) []byte {
	feed := h.feed()
	text := Render(feed, h.now())

	h.mu.Lock()
	h.reported[sessionID(body)] = statusesIn(feed)
	h.mu.Unlock()

	if text == "" {
		return emptyJSON
	}

	return envelope("SessionStart", "Quota headroom (QuotaBar):\n"+text)
}

// Prompt speaks only when a bucket got worse since the session baseline.
func (h *Hooks) Prompt(body []byte) []byte {
	feed := h.feed()
	now := h.now()
	session := sessionID(body)

	h.mu.Lock()
	previous := h.reported[session]
	if previous == nil {
		previous = map[string]monitor.Status{}
	}
	current := statusesIn(feed)
	h.reported[session] = current
	h.mu.Unlock()

	worsened := map[string]bool{}
	for key, st := range current {
		if st > previous[key] {
			worsened[key] = true
		}
	}

	var lines []string
	for _, p := range feed.Providers {
		for _, q := range p.Quotas {
			if worsened[p.ID+"/"+q.Key] {
				lines = append(lines, RenderProvider(p, now)...)
				break
			}
		}
	}

	if len(lines) == 0 {
		return emptyJSON
	}

	return envelope("UserPromptSubmit", "Quota alert (QuotaBar):\n"+strings.Join(lines, "\n"))
}
