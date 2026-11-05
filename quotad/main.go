// Command quotad is the always-on quota backend: it refreshes provider
// quotas on a background loop and serves them over a localhost HTTP feed.
//
// Skeleton: zero providers are registered yet; later jobs append to the
// providers list. Flags only override the settings path and the feed port.
package main

import (
	"context"
	"flag"
	"log/slog"
	"os"
	"os/signal"
	"syscall"
	"time"

	"quotad/feed"
	"quotad/monitor"
	"quotad/providers/claude"
	"quotad/settings"
)

func main() {
	slog.SetDefault(slog.New(slog.NewTextHandler(os.Stderr, nil)))

	settingsPath := flag.String("settings", settings.DefaultPath(), "path to settings.json")
	portOverride := flag.Int("port", 0, "override the feed port from settings")
	flag.Parse()

	// Later jobs append concrete providers here.
	providers := []monitor.Provider{claude.New()}

	loader := settings.NewLoader(*settingsPath)
	cfg := loader.Current()

	m := monitor.New(providers,
		monitor.WithIsEnabled(func(id string) bool {
			return loader.Current().ProviderEnabled(id, true)
		}),
		monitor.WithDisplayName(func(id string) string { return id }),
	)

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()

	if cfg.BackgroundSyncEnabled {
		go func() {
			interval := loader.Current().BackgroundSyncInterval
			slog.Info("background loop started", "interval", interval)
			m.StartLoop(ctx, interval)
		}()
	}

	var server *feed.Server
	if port := feedPort(cfg, *portOverride); port > 0 {
		server = &feed.Server{Monitor: m}
		server.Hooks = feed.NewHooks(
			func() feed.Feed { return feed.Build(m, time.Now()) },
			time.Now,
		)
		if err := server.Start(port); err != nil {
			slog.Error("feed server failed to start", "port", port, "err", err)
		} else {
			slog.Info("feed server listening", "addr", "127.0.0.1", "port", port)
		}
	}

	<-ctx.Done()
	slog.Info("shutting down")
	if server != nil {
		shutdown, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		if err := server.Stop(shutdown); err != nil {
			slog.Error("feed server shutdown failed", "err", err)
		}
	}
}

// feedPort resolves the listen port: the -port flag wins, then settings
// when the feed is enabled, else 0 (disabled).
func feedPort(cfg settings.Settings, override int) int {
	if override != 0 {
		return override
	}
	if cfg.FeedEnabled {
		return cfg.FeedPort
	}
	return 0
}
