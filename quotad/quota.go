// The quota subcommand: a client of the local quotad feed.
package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"net/http"
	"time"

	"quotad/feed"
	"quotad/settings"
)

// quotaTimeout bounds the GET /quotas request.
const quotaTimeout = 25 * time.Second

// runQuota fetches the feed and prints it: the raw body with --json,
// otherwise the same per-provider text the /hooks/session-start
// endpoint produces via feed.Render.
func runQuota(args []string, stdout, stderr io.Writer) int {
	fs := flag.NewFlagSet("quota", flag.ContinueOnError)
	fs.SetOutput(stderr)
	settingsPath := fs.String("settings", settings.DefaultPath(), "path to settings.json")
	portOverride := fs.Int("port", 0, "override the feed port from settings")
	jsonOut := fs.Bool("json", false, "print the raw feed body")
	if err := fs.Parse(args); err != nil {
		if err == flag.ErrHelp {
			return 0
		}
		return 2
	}
	if fs.NArg() > 0 {
		fmt.Fprintf(stderr, "quotad quota: unexpected argument %q\n", fs.Arg(0))
		return 2
	}

	port := *portOverride
	if port == 0 {
		port = settings.NewLoader(*settingsPath).Current().FeedPort
		if port == 0 {
			port = settings.DefaultFeedPort
		}
	}

	resp, err := (&http.Client{Timeout: quotaTimeout}).Get(
		fmt.Sprintf("http://127.0.0.1:%d/quotas", port))
	if err != nil {
		fmt.Fprintf(stderr, "quotad: no answer from 127.0.0.1:%d; is the quotad service running?\n", port)
		return 1
	}
	defer resp.Body.Close()

	body, err := io.ReadAll(resp.Body)
	if err != nil {
		fmt.Fprintf(stderr, "quotad: could not read feed from 127.0.0.1:%d: %v\n", port, err)
		return 1
	}
	if resp.StatusCode != http.StatusOK {
		fmt.Fprintf(stderr, "quotad: feed returned status %d from 127.0.0.1:%d\n", resp.StatusCode, port)
		return 1
	}
	var f feed.Feed
	if err := json.Unmarshal(body, &f); err != nil {
		fmt.Fprintf(stderr, "quotad: could not decode feed from 127.0.0.1:%d: %v\n", port, err)
		return 1
	}

	if *jsonOut {
		if _, err := stdout.Write(body); err != nil {
			fmt.Fprintf(stderr, "quotad: could not write output: %v\n", err)
			return 1
		}
		return 0
	}

	fmt.Fprintln(stdout, feed.Render(f, time.Now()))
	return 0
}
