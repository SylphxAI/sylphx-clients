// Package client is the provider's HTTP client for the Sylphx Resource API
// (resource-api-and-clients.md §3): bearer auth, one Idempotency-Key per
// logical call reused across retries, RFC 9457 problem bodies, and
// long-running Operation waits.
package client

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math/big"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"
)

// DefaultBaseURL is the one public API.
const DefaultBaseURL = "https://api.sylphx.com"

// Client calls the Sylphx Resource API.
type Client struct {
	BaseURL    string
	APIKey     string
	HTTP       *http.Client
	UserAgent  string
	MaxRetries int
	// Sleep waits between retries; tests replace it.
	Sleep func(ctx context.Context, d time.Duration) error
}

// New returns a client for baseURL (DefaultBaseURL when empty).
func New(baseURL, apiKey, userAgent string) *Client {
	if baseURL == "" {
		baseURL = DefaultBaseURL
	}
	return &Client{
		BaseURL:    strings.TrimRight(baseURL, "/"),
		APIKey:     apiKey,
		HTTP:       &http.Client{Timeout: 90 * time.Second},
		UserAgent:  userAgent,
		MaxRetries: 3,
		Sleep:      sleep,
	}
}

func sleep(ctx context.Context, d time.Duration) error {
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-t.C:
		return nil
	}
}

// Problem is the one error body of the API (§3.8).
type Problem struct {
	Type         string          `json:"type"`
	Title        string          `json:"title"`
	Status       int             `json:"status"`
	Detail       string          `json:"detail"`
	Instance     string          `json:"instance"`
	Code         string          `json:"code"`
	GRPCStatus   string          `json:"grpc_status"`
	Retryable    bool            `json:"retryable"`
	Effect       string          `json:"effect"`
	RetryAfterMS json.RawMessage `json:"retry_after_ms,omitempty"`
	Details      json.RawMessage `json:"details,omitempty"`
	// RequestID is the Sylphx-Request-Id header, else Instance.
	RequestID string `json:"-"`
}

func (p *Problem) Error() string {
	code := p.Code
	if code == "" {
		code = "UNKNOWN"
	}
	msg := fmt.Sprintf("%d %s: %s", p.Status, code, p.Detail)
	if p.RequestID != "" {
		msg += " (request " + p.RequestID + ")"
	}
	return msg
}

// DecodeProblem reads a problem body; a body that is not one still yields a
// Problem carrying the HTTP status and the raw text.
func DecodeProblem(status int, body []byte, requestID string) *Problem {
	p := &Problem{}
	if err := json.Unmarshal(body, p); err != nil || (p.Code == "" && p.Title == "" && p.Detail == "") {
		p = &Problem{Detail: strings.TrimSpace(string(body))}
	}
	if p.Status == 0 {
		p.Status = status
	}
	if requestID != "" {
		p.RequestID = requestID
	} else {
		p.RequestID = p.Instance
	}
	return p
}

// IsNotFound reports whether err is a NOT_FOUND problem.
func IsNotFound(err error) bool {
	var p *Problem
	return errors.As(err, &p) && (p.Status == http.StatusNotFound || p.GRPCStatus == "NOT_FOUND")
}

// Request is one logical call.
type Request struct {
	Method string
	Path   string // begins with /v1/
	Query  url.Values
	Body   any
	// IfMatch is sent as If-Match (quoted etag, §3.5).
	IfMatch string
}

// Do sends r, retrying retryable answers with full-jitter backoff, and
// decodes a JSON object response. A mutation carries one Idempotency-Key,
// the same on every retry (§3.6).
func (c *Client) Do(ctx context.Context, r Request) (map[string]any, error) {
	var payload []byte
	if r.Body != nil {
		b, err := json.Marshal(r.Body)
		if err != nil {
			return nil, err
		}
		payload = b
	}
	key := ""
	if r.Method != http.MethodGet {
		key = NewIdempotencyKey()
	}
	for attempt := 0; ; attempt++ {
		out, retryAfter, err := c.once(ctx, r, payload, key)
		if err == nil || !retryable(err) || attempt >= c.MaxRetries {
			return out, err
		}
		wait := retryAfter
		if wait < 0 {
			wait = backoff(attempt)
		}
		if serr := c.Sleep(ctx, wait); serr != nil {
			return nil, err
		}
	}
}

func retryable(err error) bool {
	var p *Problem
	if errors.As(err, &p) {
		switch p.Status {
		case 429, 502, 503, 504:
			return true
		}
		return p.Retryable
	}
	var t *TransportError
	return errors.As(err, &t)
}

// TransportError is a request that produced no response.
type TransportError struct{ Err error }

func (e *TransportError) Error() string { return "transport: " + e.Err.Error() }
func (e *TransportError) Unwrap() error { return e.Err }

func backoff(attempt int) time.Duration {
	capMS := int64(250) << min(attempt, 5)
	if capMS > 8000 {
		capMS = 8000
	}
	n, err := rand.Int(rand.Reader, big.NewInt(capMS+1))
	if err != nil {
		return time.Duration(capMS) * time.Millisecond
	}
	return time.Duration(n.Int64()) * time.Millisecond
}

func (c *Client) once(ctx context.Context, r Request, payload []byte, key string) (map[string]any, time.Duration, error) {
	u := c.BaseURL + r.Path
	if len(r.Query) > 0 {
		u += "?" + r.Query.Encode()
	}
	var body io.Reader
	if payload != nil {
		body = bytes.NewReader(payload)
	}
	req, err := http.NewRequestWithContext(ctx, r.Method, u, body)
	if err != nil {
		return nil, -1, err
	}
	req.Header.Set("Accept", "application/json")
	if payload != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	if c.APIKey != "" {
		req.Header.Set("Authorization", "Bearer "+c.APIKey)
	}
	if c.UserAgent != "" {
		req.Header.Set("User-Agent", c.UserAgent)
	}
	if key != "" {
		req.Header.Set("Idempotency-Key", key)
	}
	if r.IfMatch != "" {
		req.Header.Set("If-Match", r.IfMatch)
	}
	if dl, ok := ctx.Deadline(); ok {
		if left := time.Until(dl); left > 0 {
			req.Header.Set("Sylphx-Request-Timeout", strconv.FormatInt(left.Milliseconds(), 10))
		}
	}
	resp, err := c.HTTP.Do(req)
	if err != nil {
		if ctx.Err() != nil {
			return nil, -1, ctx.Err()
		}
		return nil, -1, &TransportError{Err: err}
	}
	defer resp.Body.Close()
	raw, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, -1, &TransportError{Err: err}
	}
	retryAfter := time.Duration(-1)
	if s := resp.Header.Get("Retry-After"); s != "" {
		if n, err := strconv.Atoi(strings.TrimSpace(s)); err == nil {
			retryAfter = time.Duration(n) * time.Second
		}
	}
	if resp.StatusCode >= 400 {
		return nil, retryAfter, DecodeProblem(resp.StatusCode, raw, resp.Header.Get("Sylphx-Request-Id"))
	}
	out := map[string]any{}
	if len(bytes.TrimSpace(raw)) > 0 {
		dec := json.NewDecoder(bytes.NewReader(raw))
		dec.UseNumber()
		if err := dec.Decode(&out); err != nil {
			return nil, -1, fmt.Errorf("decode response of %s %s: %w", r.Method, r.Path, err)
		}
	}
	return out, -1, nil
}

// Wait long-polls an Operation (§3.9) until it is done or ctx ends, and
// returns the final Operation. A done Operation carrying an error returns
// that error.
func (c *Client) Wait(ctx context.Context, op map[string]any) (map[string]any, error) {
	for {
		if done, _ := op["done"].(bool); done {
			if e, ok := op["error"].(map[string]any); ok {
				b, _ := json.Marshal(e)
				status := 0
				if n, ok := e["status"].(json.Number); ok {
					v, _ := n.Int64()
					status = int(v)
				}
				return op, DecodeProblem(status, b, "")
			}
			return op, nil
		}
		name, _ := op["name"].(string)
		if name == "" {
			return op, fmt.Errorf("operation without a name: %v", op)
		}
		next, err := c.Do(ctx, Request{
			Method: http.MethodPost,
			Path:   "/v1/" + name + ":wait",
			Query:  url.Values{"timeout": {"60s"}},
			Body:   map[string]any{},
		})
		if err != nil {
			return op, err
		}
		op = next
	}
}

// NewIdempotencyKey returns a UUIDv7.
func NewIdempotencyKey() string {
	var b [16]byte
	_, _ = rand.Read(b[:])
	binary.BigEndian.PutUint64(b[:8], uint64(time.Now().UnixMilli())<<16|binary.BigEndian.Uint64(b[:8])&0xffff)
	b[6] = b[6]&0x0f | 0x70
	b[8] = b[8]&0x3f | 0x80
	h := hex.EncodeToString(b[:])
	return h[0:8] + "-" + h[8:12] + "-" + h[12:16] + "-" + h[16:20] + "-" + h[20:]
}
