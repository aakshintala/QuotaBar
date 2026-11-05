package main

import (
	"bytes"
	"fmt"
	"net"
	"net/http"
	"net/http/httptest"
	"strconv"
	"strings"
	"testing"
)

func healthyFeedBody() string {
	return `{"generatedAt":"2026-11-05T22:08:10Z","providers":[{"id":"claude","name":"Claude","tier":"Ultra","capturedAt":"2026-11-05T22:04:18Z","ageSeconds":230,"status":"healthy","quotas":[{"key":"weekly","label":"Weekly","percentRemaining":91,"status":"healthy"},{"key":"model-fable","label":"Fable","percentRemaining":100,"status":"healthy"}]}],"disabledProviderIds":[]}`
}

// feedServer serves body at /quotas with status and returns its port.
func feedServer(t *testing.T, status int, body string) (int, func()) {
	t.Helper()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/quotas" {
			http.NotFound(w, r)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(status)
		fmt.Fprint(w, body)
	}))
	t.Cleanup(srv.Close)
	port := srv.Listener.Addr().(*net.TCPAddr).Port
	return port, srv.Close
}

func portArg(port int) string { return strconv.Itoa(port) }

func TestQuotaTextUsesSessionStartFormatter(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	port, _ := feedServer(t, http.StatusOK, healthyFeedBody())
	var stdout, stderr bytes.Buffer
	if code := run([]string{"quotad", "quota", "-port", portArg(port)}, &stdout, &stderr); code != 0 {
		t.Fatalf("exit = %d, stderr = %q", code, stderr.String())
	}
	want := "claude (Ultra) - weekly 91% · fable 100%\n"
	if stdout.String() != want {
		t.Errorf("stdout = %q, want %q", stdout.String(), want)
	}
}

func TestQuotaJSONPrintsBodyAsIs(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	body := healthyFeedBody()
	port, _ := feedServer(t, http.StatusOK, body)
	var stdout, stderr bytes.Buffer
	if code := run([]string{"quotad", "quota", "-port", portArg(port), "--json"}, &stdout, &stderr); code != 0 {
		t.Fatalf("exit = %d, stderr = %q", code, stderr.String())
	}
	if stdout.String() != body {
		t.Errorf("stdout = %q, want body as-is %q", stdout.String(), body)
	}
}

func TestQuotaUnreachable(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	port := ln.Addr().(*net.TCPAddr).Port
	ln.Close()
	var stdout, stderr bytes.Buffer
	if code := run([]string{"quotad", "quota", "-port", portArg(port)}, &stdout, &stderr); code != 1 {
		t.Fatalf("exit = %d, want 1 (stderr = %q)", code, stderr.String())
	}
	want := fmt.Sprintf("quotad: no answer from 127.0.0.1:%d; is the quotad service running?\n", port)
	if stderr.String() != want {
		t.Errorf("stderr = %q, want %q", stderr.String(), want)
	}
}

func TestQuotaNon200(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	port, _ := feedServer(t, http.StatusInternalServerError, "boom")
	var stdout, stderr bytes.Buffer
	if code := run([]string{"quotad", "quota", "-port", portArg(port)}, &stdout, &stderr); code != 1 {
		t.Fatalf("exit = %d, want 1 (stderr = %q)", code, stderr.String())
	}
	if !strings.HasPrefix(stderr.String(), "quotad: ") {
		t.Errorf("stderr = %q, want a quotad stderr line", stderr.String())
	}
}

func TestQuotaUndecodable(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	port, _ := feedServer(t, http.StatusOK, "this is not json")
	var stdout, stderr bytes.Buffer
	if code := run([]string{"quotad", "quota", "-port", portArg(port)}, &stdout, &stderr); code != 1 {
		t.Fatalf("exit = %d, want 1 (stderr = %q)", code, stderr.String())
	}
	if !strings.HasPrefix(stderr.String(), "quotad: ") {
		t.Errorf("stderr = %q, want a quotad stderr line", stderr.String())
	}
}

func TestQuotaRejectsPositionalArgs(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	var stdout, stderr bytes.Buffer
	if code := run([]string{"quotad", "quota", "extra"}, &stdout, &stderr); code != 2 {
		t.Fatalf("exit = %d, want 2 (stderr = %q)", code, stderr.String())
	}
}

func TestAiquotaDispatchBehavesAsQuota(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	body := healthyFeedBody()
	port, _ := feedServer(t, http.StatusOK, body)
	var stdout, stderr bytes.Buffer
	if code := run([]string{"/home/u/.local/bin/aiquota", "-port", portArg(port)}, &stdout, &stderr); code != 0 {
		t.Fatalf("text exit = %d, stderr = %q", code, stderr.String())
	}
	if want := "claude (Ultra) - weekly 91% · fable 100%\n"; stdout.String() != want {
		t.Errorf("text stdout = %q, want %q", stdout.String(), want)
	}
	stdout.Reset()
	stderr.Reset()
	if code := run([]string{"aiquota", "-port", portArg(port), "--json"}, &stdout, &stderr); code != 0 {
		t.Fatalf("json exit = %d, stderr = %q", code, stderr.String())
	}
	if stdout.String() != body {
		t.Errorf("json stdout = %q, want %q", stdout.String(), body)
	}
}

func TestUnknownSubcommand(t *testing.T) {
	var stdout, stderr bytes.Buffer
	if code := run([]string{"quotad", "bogus"}, &stdout, &stderr); code != 2 {
		t.Fatalf("exit = %d, want 2 (stderr = %q)", code, stderr.String())
	}
}
