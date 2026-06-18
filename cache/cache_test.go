package cache

import (
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// get issues a GET through rt and returns the body.
func get(t *testing.T, rt http.RoundTripper, url string) string {
	t.Helper()
	req, err := http.NewRequest(http.MethodGet, url, nil)
	if err != nil {
		t.Fatal(err)
	}
	resp, err := rt.RoundTrip(req)
	if err != nil {
		t.Fatalf("round trip: %v", err)
	}
	defer func() { _ = resp.Body.Close() }()
	b, err := io.ReadAll(resp.Body)
	if err != nil {
		t.Fatal(err)
	}
	return string(b)
}

func TestCacheServesSecondRequestFromDisk(t *testing.T) {
	var hits int
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		hits++
		_, _ = io.WriteString(w, "payload")
	}))
	defer srv.Close()

	rt := Wrap(srv.Client().Transport, t.TempDir(), time.Hour, nil)

	if got := get(t, rt, srv.URL); got != "payload" {
		t.Fatalf("first body = %q", got)
	}
	if got := get(t, rt, srv.URL); got != "payload" {
		t.Fatalf("second body = %q", got)
	}
	if hits != 1 {
		t.Fatalf("server hit %d times, want 1 (second served from cache)", hits)
	}
}

func TestCachePostKeyedByBody(t *testing.T) {
	// InnerTube reads are POSTs with a JSON body; the cache key must include the
	// body, and the body must survive being read for the key so the request still
	// reaches the server on a miss.
	var bodies []string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		b, _ := io.ReadAll(r.Body)
		bodies = append(bodies, string(b))
		_, _ = io.WriteString(w, "ok:"+string(b))
	}))
	defer srv.Close()

	rt := Wrap(srv.Client().Transport, t.TempDir(), time.Hour, nil)
	post := func(body string) string {
		req, _ := http.NewRequest(http.MethodPost, srv.URL, strings.NewReader(body))
		resp, err := rt.RoundTrip(req)
		if err != nil {
			t.Fatal(err)
		}
		defer func() { _ = resp.Body.Close() }()
		b, _ := io.ReadAll(resp.Body)
		return string(b)
	}

	if got := post(`{"id":1}`); got != `ok:{"id":1}` {
		t.Fatalf("first post body echoed = %q", got)
	}
	if got := post(`{"id":1}`); got != `ok:{"id":1}` { // same body -> cache hit
		t.Fatalf("cached post body = %q", got)
	}
	if got := post(`{"id":2}`); got != `ok:{"id":2}` { // different body -> miss
		t.Fatalf("distinct post body = %q", got)
	}
	if len(bodies) != 2 {
		t.Fatalf("server saw %d bodies %v, want 2 (the repeat was cached)", len(bodies), bodies)
	}
}

func TestCacheRefetchesWhenStale(t *testing.T) {
	var hits int
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		hits++
		_, _ = io.WriteString(w, "payload")
	}))
	defer srv.Close()

	// A zero TTL resolves to DefaultTTL, so use a tiny negative-free small window by
	// writing then waiting past it.
	rt := Wrap(srv.Client().Transport, t.TempDir(), time.Nanosecond, nil)

	get(t, rt, srv.URL)
	time.Sleep(time.Millisecond)
	get(t, rt, srv.URL)
	if hits != 2 {
		t.Fatalf("server hit %d times, want 2 (stale entry re-fetched)", hits)
	}
}

func TestCacheRespectsAllowPolicy(t *testing.T) {
	var hits int
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		hits++
		_, _ = io.WriteString(w, "payload")
	}))
	defer srv.Close()

	// A policy that refuses everything means nothing is cached.
	deny := func(*http.Request) bool { return false }
	rt := Wrap(srv.Client().Transport, t.TempDir(), time.Hour, deny)

	get(t, rt, srv.URL)
	get(t, rt, srv.URL)
	if hits != 2 {
		t.Fatalf("server hit %d times, want 2 (policy denied caching)", hits)
	}
}

func TestCacheSkipsRangeRequests(t *testing.T) {
	var hits int
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		hits++
		_, _ = io.WriteString(w, "payload")
	}))
	defer srv.Close()

	rt := Wrap(srv.Client().Transport, t.TempDir(), time.Hour, nil)
	for range 2 {
		req, _ := http.NewRequest(http.MethodGet, srv.URL, nil)
		req.Header.Set("Range", "bytes=0-3")
		resp, err := rt.RoundTrip(req)
		if err != nil {
			t.Fatal(err)
		}
		_ = resp.Body.Close()
	}
	if hits != 2 {
		t.Fatalf("server hit %d times, want 2 (ranged requests are never cached)", hits)
	}
}

func TestCacheSkipsNon200(t *testing.T) {
	var hits int
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		hits++
		w.WriteHeader(http.StatusInternalServerError)
		_, _ = io.WriteString(w, "boom")
	}))
	defer srv.Close()

	dir := t.TempDir()
	rt := Wrap(srv.Client().Transport, dir, time.Hour, nil)
	get(t, rt, srv.URL)
	get(t, rt, srv.URL)
	if hits != 2 {
		t.Fatalf("server hit %d times, want 2 (errors are not cached)", hits)
	}
	// No cache file should have been written for the error.
	entries, _ := os.ReadDir(dir)
	for _, e := range entries {
		if !strings.HasSuffix(e.Name(), ".tmp") && filepath.Ext(e.Name()) == "" {
			t.Errorf("unexpected cache file for an error response: %s", e.Name())
		}
	}
}
