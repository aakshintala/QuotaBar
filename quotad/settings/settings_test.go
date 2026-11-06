package settings

import (
	"os"
	"path/filepath"
	"testing"
	"time"
)

func writeFile(t *testing.T, content string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "settings.json")
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	return path
}

func TestMissingFileYieldsDefaults(t *testing.T) {
	s := NewLoader(filepath.Join(t.TempDir(), "nonexistent.json")).Current()
	if s.BackgroundSyncEnabled {
		t.Error("BackgroundSyncEnabled should default to false")
	}
	if s.BackgroundSyncInterval != 600*time.Second {
		t.Errorf("BackgroundSyncInterval = %v, want 600s", s.BackgroundSyncInterval)
	}
	if s.FeedEnabled {
		t.Error("FeedEnabled should default to false")
	}
	if s.FeedPort != 8787 {
		t.Errorf("FeedPort = %d, want 8787", s.FeedPort)
	}
	if !s.ProviderEnabled("claude", true) || s.ProviderEnabled("claude", false) {
		t.Error("unknown provider should return the default")
	}
}

func TestParsesKnownKeys(t *testing.T) {
	path := writeFile(t, `{
		"app": {"backgroundSyncEnabled": true, "backgroundSyncInterval": 300},
		"feed": {"enabled": true, "port": 9999},
		"providers": {"claude": {"isEnabled": false}, "codex": {}}
	}`)
	s := NewLoader(path).Current()
	if !s.BackgroundSyncEnabled {
		t.Error("BackgroundSyncEnabled should be true")
	}
	if s.BackgroundSyncInterval != 300*time.Second {
		t.Errorf("BackgroundSyncInterval = %v, want 300s", s.BackgroundSyncInterval)
	}
	if !s.FeedEnabled || s.FeedPort != 9999 {
		t.Errorf("feed = (%v, %d), want (true, 9999)", s.FeedEnabled, s.FeedPort)
	}
	if s.ProviderEnabled("claude", true) {
		t.Error("claude should be disabled")
	}
	if !s.ProviderEnabled("codex", true) {
		t.Error("codex with absent isEnabled should return the default")
	}
}

func TestBadJSONYieldsDefaults(t *testing.T) {
	s := NewLoader(writeFile(t, `{not json`)).Current()
	if s.FeedPort != 8787 || s.FeedEnabled {
		t.Errorf("bad JSON should yield defaults, got %+v", s)
	}
}

func TestMigratesMCPSection(t *testing.T) {
	path := writeFile(t, `{"mcp": {"enabled": true, "port": 1234}}`)
	s := NewLoader(path).Current()
	if !s.FeedEnabled || s.FeedPort != 1234 {
		t.Errorf("mcp section should migrate to feed, got (%v, %d)", s.FeedEnabled, s.FeedPort)
	}
}

func TestRereadsWhenMtimeChanges(t *testing.T) {
	path := writeFile(t, `{"feed": {"port": 1111}}`)
	loader := NewLoader(path)
	if got := loader.Current().FeedPort; got != 1111 {
		t.Fatalf("FeedPort = %d, want 1111", got)
	}
	if err := os.WriteFile(path, []byte(`{"feed": {"port": 2222}}`), 0o644); err != nil {
		t.Fatal(err)
	}
	// Filesystems may keep mtime granularity coarse; force a newer mtime.
	future := time.Now().Add(2 * time.Second)
	if err := os.Chtimes(path, future, future); err != nil {
		t.Fatal(err)
	}
	if got := loader.Current().FeedPort; got != 2222 {
		t.Errorf("after mtime change FeedPort = %d, want 2222", got)
	}
}
