package settings

import (
	"encoding/json"
	"os"
	"path/filepath"
	"sync"
	"time"
)

const (
	DefaultFeedPort              = 8787
	DefaultBackgroundSyncSeconds = 600
)

type Settings struct {
	BackgroundSyncEnabled  bool
	BackgroundSyncInterval time.Duration
	FeedEnabled            bool
	FeedPort               int
	providers              map[string]*bool
}

func (s Settings) ProviderEnabled(id string, def bool) bool {
	if v, ok := s.providers[id]; ok && v != nil {
		return *v
	}
	return def
}
func defaults() Settings {
	return Settings{BackgroundSyncInterval: DefaultBackgroundSyncSeconds * time.Second, FeedPort: DefaultFeedPort, providers: map[string]*bool{}}
}

type fileLeaf struct {
	BackgroundSyncEnabled  *bool    `json:"backgroundSyncEnabled"`
	BackgroundSyncInterval *float64 `json:"backgroundSyncInterval"`
	Enabled                *bool    `json:"enabled"`
	Port                   *int     `json:"port"`
	IsEnabled              *bool    `json:"isEnabled"`
}
type fileRoot struct {
	App       fileLeaf            `json:"app"`
	Feed      *fileLeaf           `json:"feed"`
	MCP       *fileLeaf           `json:"mcp"`
	Providers map[string]fileLeaf `json:"providers"`
}

func parse(data []byte) Settings {
	s := defaults()
	var root fileRoot
	if json.Unmarshal(data, &root) != nil {
		return s
	}
	if v := root.App.BackgroundSyncEnabled; v != nil {
		s.BackgroundSyncEnabled = *v
	}
	if v := root.App.BackgroundSyncInterval; v != nil {
		s.BackgroundSyncInterval = time.Duration(*v * float64(time.Second))
	}
	feed := root.Feed
	if feed == nil {
		feed = root.MCP // migrated from the old `mcp` section
	}
	if feed != nil {
		if v := feed.Enabled; v != nil {
			s.FeedEnabled = *v
		}
		if v := feed.Port; v != nil {
			s.FeedPort = *v
		}
	}
	for id, p := range root.Providers {
		s.providers[id] = p.IsEnabled
	}
	return s
}

type Loader struct {
	path   string
	mu     sync.Mutex
	mtime  time.Time
	cached Settings
	loaded bool
}

func NewLoader(path string) *Loader { return &Loader{path: path} }
func DefaultPath() string {
	home, err := os.UserHomeDir()
	if err != nil {
		return ""
	}
	return filepath.Join(home, ".quotabar", "settings.json")
}
func (l *Loader) Current() Settings {
	l.mu.Lock()
	defer l.mu.Unlock()
	info, err := os.Stat(l.path)
	if err != nil {
		l.cached, l.loaded, l.mtime = defaults(), true, time.Time{}
		return l.cached
	}
	if l.loaded && info.ModTime().Equal(l.mtime) {
		return l.cached
	}
	data, err := os.ReadFile(l.path)
	if err != nil {
		l.cached, l.loaded, l.mtime = defaults(), true, info.ModTime()
		return l.cached
	}
	l.cached, l.loaded, l.mtime = parse(data), true, info.ModTime()
	return l.cached
}
