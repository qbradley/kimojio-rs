package main

import (
	"bytes"
	"io"
	"net/http"
	"net/http/httptest"
	"sync/atomic"
	"testing"
	"time"
)

func TestHistogramBoundsAndMerge(t *testing.T) {
	var h histogram
	for i := 0; i < 100; i++ {
		h.add(time.Millisecond)
	}
	if q := h.quantile(.99); q < 1 || q > 1.032 {
		t.Fatalf("quantile %f", q)
	}
}
func TestHTTPWrongPayloadCannotSucceed(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Length", "3")
		w.Write([]byte("bad"))
	}))
	defer server.Close()
	result, err := loadHTTP([]string{"--url", server.URL, "--size", "3", "--duration", "10ms"})
	if err != nil || result.Successes != 0 || len(result.Errors) != 1 || result.Attempts != 1 {
		t.Fatalf("result %+v, err %v", result, err)
	}
}
func TestHTTPFixedIsNotChunkedFixture(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Length", "13")
		w.Write([]byte("first\nsecond\n"))
	}))
	defer server.Close()
	result, err := loadHTTP([]string{"--url", server.URL, "--size", "13", "--framing", "trailers", "--duration", "10ms"})
	if err != nil || result.Successes != 0 || len(result.Errors) != 1 {
		t.Fatalf("false chunked success: %+v %v", result, err)
	}
}
func TestChunkedEchoValidatesUploadAndResponse(t *testing.T) {
	var observed atomic.Bool
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method == "POST" && r.ContentLength == -1 && len(r.TransferEncoding) == 1 && r.TransferEncoding[0] == "chunked" {
			observed.Store(true)
		}
		if err := http.NewResponseController(w).EnableFullDuplex(); err != nil {
			t.Error(err)
			return
		}
		w.Header().Set("Connection", "close")
		w.WriteHeader(200)
		w.(http.Flusher).Flush()
		_, err := io.Copy(w, r.Body)
		if err != nil {
			t.Error(err)
		}
	}))
	defer server.Close()
	result, err := loadHTTP([]string{"--url", server.URL, "--size", "65536", "--framing", "chunked-echo", "--fresh", "--duration", "10ms"})
	if err != nil || len(result.Errors) != 0 || result.Successes == 0 || !observed.Load() {
		t.Fatalf("chunked echo failed: %v, %v, %d", err, result.Errors, result.Successes)
	}
	if result.Bytes != result.Successes*65536 {
		t.Fatal("wrong validated payload count")
	}
}

func TestChunkedEchoRejectsFixedResponse(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.Copy(io.Discard, r.Body)
		w.Header().Set("Connection", "close")
		w.Header().Set("Content-Length", "65536")
		w.Write(bytes.Repeat([]byte{'x'}, 65536))
	}))
	defer server.Close()
	result, err := loadHTTP([]string{"--url", server.URL, "--size", "65536", "--framing", "chunked-echo", "--fresh", "--duration", "10ms"})
	if err != nil || result.Successes != 0 || len(result.Errors) == 0 {
		t.Fatal("fixed response incorrectly accepted as chunked echo")
	}
}

func TestBoundedConfiguration(t *testing.T) {
	for _, args := range [][]string{
		{"--url", "http://example.com"},
		{"--url", "http://127.0.0.1:1", "--duration", "1h"},
		{"--url", "http://127.0.0.1:1", "--size", "-1"},
	} {
		if _, err := parseLoad(args, false); err == nil {
			t.Fatalf("accepted %v", args)
		}

	}
}

func TestReferenceAdvertisesRequestCap(t *testing.T) {
	server := httptest.NewUnstartedServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Length", "1")
		w.Write([]byte("x"))
	}))
	applyRequestCap(server.Config, 2)
	server.Start()
	defer server.Close()
	client := server.Client()
	for i := 0; i < 3; i++ {
		response, err := client.Get(server.URL)
		if err != nil {
			t.Fatal(err)
		}
		_, err = io.Copy(io.Discard, response.Body)
		response.Body.Close()
		if err != nil {
			t.Fatal(err)
		}
		if response.Close != (i == 1) {
			t.Fatalf("response %d close=%v", i, response.Close)
		}
	}
}
