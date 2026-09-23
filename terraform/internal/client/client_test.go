package client

import (
	"context"
	"net/http"
	"net/http/httptest"
	"regexp"
	"testing"
	"time"
)

func TestDecodeProblem(t *testing.T) {
	p := DecodeProblem(409, []byte(`{"type":"https://sylphx.com/docs/errors/etag-mismatch","title":"t","status":409,
		"detail":"changed","instance":"req_1","code":"ETAG_MISMATCH","grpc_status":"ABORTED","retryable":true,"effect":"none","retry_after_ms":null}`), "")
	if p.Code != "ETAG_MISMATCH" || p.Status != 409 || !p.Retryable || p.RequestID != "req_1" || p.GRPCStatus != "ABORTED" {
		t.Fatalf("decoded %+v", p)
	}
	if got := p.Error(); got != "409 ETAG_MISMATCH: changed (request req_1)" {
		t.Fatalf("Error() = %q", got)
	}
	raw := DecodeProblem(502, []byte("bad gateway"), "req_h")
	if raw.Status != 502 || raw.Detail != "bad gateway" || raw.RequestID != "req_h" {
		t.Fatalf("non-problem body: %+v", raw)
	}
	if !IsNotFound(DecodeProblem(404, []byte(`{"code":"RESOURCE_NOT_FOUND","grpc_status":"NOT_FOUND"}`), "")) {
		t.Fatal("404 is not found")
	}
}

func TestRetriesKeepTheIdempotencyKey(t *testing.T) {
	var keys []string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		keys = append(keys, r.Header.Get("Idempotency-Key"))
		if r.Header.Get("Authorization") != "Bearer k" {
			t.Errorf("auth header %q", r.Header.Get("Authorization"))
		}
		if len(keys) == 1 {
			w.Header().Set("Content-Type", "application/problem+json")
			w.WriteHeader(503)
			_, _ = w.Write([]byte(`{"code":"UNAVAILABLE","status":503,"retryable":true}`))
			return
		}
		_, _ = w.Write([]byte(`{"name":"orgs/o"}`))
	}))
	defer srv.Close()
	c := New(srv.URL, "k", "test")
	c.Sleep = func(context.Context, time.Duration) error { return nil }
	out, err := c.Do(context.Background(), Request{Method: http.MethodPost, Path: "/v1/orgs", Body: map[string]any{}})
	if err != nil || out["name"] != "orgs/o" {
		t.Fatalf("out %v err %v", out, err)
	}
	if len(keys) != 2 || keys[0] == "" || keys[0] != keys[1] {
		t.Fatalf("keys %v", keys)
	}
	if !regexp.MustCompile(`^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$`).MatchString(keys[0]) {
		t.Fatalf("not a UUIDv7: %s", keys[0])
	}
}

func TestNonRetryableProblemReturnsAtOnce(t *testing.T) {
	n := 0
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		n++
		w.Header().Set("Sylphx-Request-Id", "req_x")
		w.WriteHeader(400)
		_, _ = w.Write([]byte(`{"code":"INVALID_FIELD","status":400,"detail":"bad"}`))
	}))
	defer srv.Close()
	_, err := New(srv.URL, "", "").Do(context.Background(), Request{Method: http.MethodGet, Path: "/v1/x"})
	p, ok := err.(*Problem)
	if !ok || p.Code != "INVALID_FIELD" || p.RequestID != "req_x" || n != 1 {
		t.Fatalf("err %v n %d", err, n)
	}
}
