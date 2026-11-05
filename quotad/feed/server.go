// localhost HTTP server for package feed.
package feed

import (
	"context"
	"encoding/json"
	"io"
	"net"
	"net/http"
	"quotad/monitor"
	"strconv"
	"strings"
	"time"
)

// RefreshTimeout bounds the monitor refresh behind GET /quotas.
const RefreshTimeout = 20 * time.Second

// maxHookBodyBytes caps hook request bodies, mirroring the Swift server's
// maxRequestBytes: a larger body is rejected with 413, never truncated.
const maxHookBodyBytes = 1 << 20

// Server serves the quota feed and hooks on 127.0.0.1.
type Server struct {
	Monitor *monitor.Monitor
	Hooks   *Hooks
	Now     func() time.Time
	http    *http.Server
}

func loopbackHost(host string) bool {
	name := host
	if i := strings.IndexByte(host, ':'); i >= 0 {
		name = host[:i]
	}

	return name == "127.0.0.1" || name == "localhost"
}

func (s *Server) now() time.Time {
	if s.Now != nil {
		return s.Now()
	}

	return time.Now()
}

func (s *Server) currentFeed(ctx context.Context) []byte {
	refresh, cancel := context.WithTimeout(ctx, RefreshTimeout)
	defer cancel()

	s.Monitor.Refresh(refresh, false)

	data, err := json.Marshal(Build(s.Monitor, s.now()))
	if err != nil {
		return []byte("{}")
	}

	return data
}

// ServeHTTP routes /quotas and the hook endpoints, rejecting non-loopback
// Host values so a DNS-rebinding page cannot read quota data.
func (s *Server) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	if !loopbackHost(r.Host) {
		http.Error(w, "Forbidden", http.StatusForbidden)
		return
	}

	var body []byte

	switch r.URL.Path {
	case "/quotas":
		if r.Method != http.MethodGet {
			http.Error(w, "Method Not Allowed", http.StatusMethodNotAllowed)
			return
		}
		body = s.currentFeed(r.Context())

	case "/hooks/session-start", "/hooks/prompt":
		if r.Method != http.MethodPost {
			http.Error(w, "Method Not Allowed", http.StatusMethodNotAllowed)
			return
		}
		if s.Hooks == nil {
			http.Error(w, "Not Found", http.StatusNotFound)
			return
		}

		// Hook bodies carry the user's prompt, so the limit is generous;
		// beyond it the request is rejected instead of silently truncated.
		hookBody, _ := io.ReadAll(io.LimitReader(r.Body, maxHookBodyBytes+1))
		if len(hookBody) > maxHookBodyBytes {
			http.Error(w, "Content Too Large", http.StatusRequestEntityTooLarge)
			return
		}

		if r.URL.Path == "/hooks/session-start" {
			body = s.Hooks.SessionStart(hookBody)
		} else {
			body = s.Hooks.Prompt(hookBody)
		}

	default:
		http.Error(w, "Not Found", http.StatusNotFound)
		return
	}

	w.Header().Set("Content-Type", "application/json")
	w.Write(body)
}

// Start binds 127.0.0.1:port and serves in the background.
func (s *Server) Start(port int) error {
	listener, err := net.Listen("tcp", "127.0.0.1:"+strconv.Itoa(port))
	if err != nil {
		return err
	}

	s.http = &http.Server{Handler: s}
	go s.http.Serve(listener)

	return nil
}

// Stop shuts the server down.
func (s *Server) Stop(ctx context.Context) error {
	if s.http == nil {
		return nil
	}

	return s.http.Shutdown(ctx)
}
