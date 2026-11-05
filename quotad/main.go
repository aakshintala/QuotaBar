// Command quotad is the always-on quota backend: it refreshes provider
// quotas on a background loop and serves them over a localhost HTTP feed.
//
// With no subcommand it runs the server. It also has client and setup
// subcommands: quota, install and uninstall. Invoked as aiquota it
// behaves as quotad quota.
package main

import (
	"context"
	"flag"
	"fmt"
	"io"
	"log/slog"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"

	"quotad/feed"
	"quotad/monitor"
	"quotad/providers/claude"
	"quotad/providers/codex"
	"quotad/providers/cursor"
	"quotad/providers/muse"
	"quotad/providers/opencode"
	"quotad/settings"
)

func main() {
	os.Exit(run(os.Args, os.Stdout, os.Stderr))
}

// run dispatches on argv[0] and the subcommand, returning the exit code.
func run(argv []string, stdout, stderr io.Writer) int {
	if len(argv) == 0 {
		printUsage(stderr)
		return 2
	}
	if filepath.Base(argv[0]) == "aiquota" {
		return runQuota(argv[1:], stdout, stderr)
	}
	if len(argv) > 1 {
		switch argv[1] {
		case "quota":
			return runQuota(argv[2:], stdout, stderr)
		case "install":
			return runInstallCommand(stdout, stderr)
		case "uninstall":
			return runUninstallCommand(stdout, stderr)
		}
		if len(argv[1]) > 0 && argv[1][0] != '-' {
			fmt.Fprintf(stderr, "quotad: unknown subcommand %q\n", argv[1])
			printUsage(stderr)
			return 2
		}
	}
	return runServer(argv[1:], stderr)
}

func printUsage(w io.Writer) {
	fmt.Fprintln(w, "usage: quotad [-settings path] [-port N] [-log path]")
	fmt.Fprintln(w, "       quotad quota [--json] [-settings path] [-port N]")
	fmt.Fprintln(w, "       quotad install | uninstall (Linux only)")
}

// runServer keeps today's behaviour: serve the feed until signalled.
func runServer(args []string, stderr io.Writer) int {
	fs := flag.NewFlagSet("quotad", flag.ContinueOnError)
	fs.SetOutput(stderr)
	settingsPath := fs.String("settings", settings.DefaultPath(), "path to settings.json")
	portOverride := fs.Int("port", 0, "override the feed port from settings")
	logPath := fs.String("log", "", "append logs to file instead of stderr (a leading ~/ expands to the home dir)")
	if err := fs.Parse(args); err != nil {
		if err == flag.ErrHelp {
			return 0
		}
		return 2
	}

	slog.SetDefault(slog.New(slog.NewTextHandler(os.Stderr, nil)))
	if *logPath != "" {
		f, err := openLogFile(expandHome(*logPath))
		if err != nil {
			fmt.Fprintf(stderr, "quotad: cannot open log file: %v\n", err)
			return 1
		}
		slog.SetDefault(slog.New(slog.NewTextHandler(f, nil)))
	}

	providers := []monitor.Provider{
		claude.New(),
		codex.New(codex.Config{}),
		cursor.New(),
		opencode.New(),
		muse.New(),
	}

	loader := settings.NewLoader(*settingsPath)
	cfg := loader.Current()

	m := monitor.New(providers,
		monitor.WithIsEnabled(func(id string) bool {
			return loader.Current().ProviderEnabled(id, true)
		}),
		monitor.WithDisplayName(displayName),
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
	return 0
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

// displayName is the human name the feed and hooks show for a provider id.
func displayName(id string) string {
	switch id {
	case "claude":
		return "Claude"
	case "codex":
		return "Codex"
	case "cursor":
		return "Cursor"
	case "opencode-go":
		return "OpenCode Go"
	case "muse":
		return "Muse"
	}
	return id
}
