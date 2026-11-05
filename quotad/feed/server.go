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

const RefreshTimeout = 20 * time.Second

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
		hookBody, _ := io.ReadAll(io.LimitReader(r.Body, 1<<20))
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
func (s *Server) Start(port int) error {
	listener, err := net.Listen("tcp", "127.0.0.1:"+strconv.Itoa(port))
	if err != nil {
		return err
	}
	s.http = &http.Server{Handler: s}
	go s.http.Serve(listener)
	return nil
}
func (s *Server) Stop(ctx context.Context) error {
	if s.http == nil {
		return nil
	}
	return s.http.Shutdown(ctx)
}
