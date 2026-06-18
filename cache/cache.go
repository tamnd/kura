// Package cache is a small, content-addressed on-disk HTTP response cache that
// wraps a RoundTripper. kura reads the free InnerTube surface with repeated,
// effectively pure requests (the same browse or player call yields the same
// answer within a freshness window), so caching those responses on disk makes a
// re-run, a re-render, or dev iteration fast and offline-friendly. The cache is
// shared across repositories (it lives in the user cache directory, not inside an
// archive) and is advisory: deleting it only costs a re-fetch. It is bypassed
// wholesale with --no-cache.
package cache

import (
	"bufio"
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"net/http"
	"net/http/httputil"
	"os"
	"path/filepath"
	"time"
)

// DefaultTTL is the freshness window for a cached response when no other is set.
// It is short enough that an incremental `add` within a session sees recent
// uploads after it expires, and --no-cache always forces fresh.
const DefaultTTL = time.Hour

// magic prefixes a cache file so a stray or foreign file is ignored rather than
// misread as a response.
const magic = "kuracache1"

// transport is the caching RoundTripper. base does the real fetching; allow is an
// optional policy that decides, per request, whether a response may be cached (the
// CLI uses it to cache only InnerTube reads and never stream or image bytes).
type transport struct {
	base  http.RoundTripper
	dir   string
	ttl   time.Duration
	allow func(*http.Request) bool
}

// Wrap returns a RoundTripper that serves fresh cached responses for eligible
// requests and stores new ones under dir. base is the underlying transport (the
// real network); a nil base uses http.DefaultTransport. A non-positive ttl uses
// DefaultTTL. allow is an optional per-request policy hook layered on top of the
// generic eligibility rules (idempotent method, no Range); a nil allow caches
// every eligible request.
func Wrap(base http.RoundTripper, dir string, ttl time.Duration, allow func(*http.Request) bool) http.RoundTripper {
	if base == nil {
		base = http.DefaultTransport
	}
	if ttl <= 0 {
		ttl = DefaultTTL
	}
	return &transport{base: base, dir: dir, ttl: ttl, allow: allow}
}

// RoundTrip serves a fresh cached response when one exists, else fetches, stores a
// successful response, and returns it. A cache read or write failure is never
// fatal: the request falls through to the network.
func (t *transport) RoundTrip(req *http.Request) (*http.Response, error) {
	if !t.cacheable(req) {
		return t.base.RoundTrip(req)
	}
	key, body, err := requestKey(req)
	if err != nil {
		return t.base.RoundTrip(req)
	}
	// Restore the body the key computation consumed so the underlying transport can
	// still send it.
	if body != nil {
		req.Body = io.NopCloser(bytes.NewReader(body))
		req.ContentLength = int64(len(body))
		req.GetBody = func() (io.ReadCloser, error) { return io.NopCloser(bytes.NewReader(body)), nil }
	}

	if resp, ok := t.load(key, req); ok {
		return resp, nil
	}

	resp, err := t.base.RoundTrip(req)
	if err != nil {
		return resp, err
	}
	if resp.StatusCode == http.StatusOK {
		t.store(key, resp) // rewrites resp.Body to a fresh reader on success
	}
	return resp, nil
}

// cacheable reports whether a request's response may be cached: an idempotent read
// (GET or POST, since InnerTube reads are POSTs with a body) with no Range header,
// passing the optional policy hook. A ranged request (a stream download) is never
// cached.
func (t *transport) cacheable(req *http.Request) bool {
	if req.Method != http.MethodGet && req.Method != http.MethodPost {
		return false
	}
	if req.Header.Get("Range") != "" {
		return false
	}
	if t.allow != nil && !t.allow(req) {
		return false
	}
	return true
}

// requestKey is the content address of a request: sha256 over the method, URL, and
// body, so two identical InnerTube calls resolve to one cache file. It returns the
// body bytes it read so the caller can restore them.
func requestKey(req *http.Request) (string, []byte, error) {
	var body []byte
	if req.Body != nil {
		b, err := io.ReadAll(req.Body)
		_ = req.Body.Close()
		if err != nil {
			return "", nil, err
		}
		body = b
	}
	h := sha256.New()
	_, _ = io.WriteString(h, req.Method)
	h.Write([]byte{0})
	_, _ = io.WriteString(h, req.URL.String())
	h.Write([]byte{0})
	h.Write(body)
	return hex.EncodeToString(h.Sum(nil)), body, nil
}

// load returns a cached response when one exists and is still within the TTL.
func (t *transport) load(key string, req *http.Request) (*http.Response, bool) {
	data, err := os.ReadFile(filepath.Join(t.dir, key))
	if err != nil {
		return nil, false
	}
	head, rest, ok := bytes.Cut(data, []byte{'\n'})
	if !ok {
		return nil, false
	}
	var stamp int64
	if _, err := fmt.Sscanf(string(head), magic+" %d", &stamp); err != nil {
		return nil, false
	}
	if time.Since(time.Unix(0, stamp)) > t.ttl {
		return nil, false // stale: re-fetch
	}
	resp, err := http.ReadResponse(bufio.NewReader(bytes.NewReader(rest)), req)
	if err != nil {
		return nil, false
	}
	return resp, true
}

// store writes a response to the cache atomically (temp file then rename), with a
// timestamp header for the freshness check. DumpResponse consumes and restores the
// response body, so the caller's response is still readable afterward.
func (t *transport) store(key string, resp *http.Response) {
	dump, err := httputil.DumpResponse(resp, true)
	if err != nil {
		return
	}
	if err := os.MkdirAll(t.dir, 0o755); err != nil {
		return
	}
	var buf bytes.Buffer
	fmt.Fprintf(&buf, "%s %d\n", magic, time.Now().UnixNano())
	buf.Write(dump)

	final := filepath.Join(t.dir, key)
	tmp := final + ".tmp"
	if err := os.WriteFile(tmp, buf.Bytes(), 0o644); err != nil {
		return
	}
	_ = os.Rename(tmp, final)
}
