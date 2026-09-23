package main

import (
	"bytes"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func fixtureRequest(f fixture, body []byte) *http.Request {
	r := httptest.NewRequest(f.Method, f.Path, bytes.NewReader(body))
	for _, h := range f.RequestHeaders {
		if h[0] == "host" {
			r.Host = h[1]
		} else {
			r.Header.Set(h[0], h[1])
		}
	}
	return r
}
func TestFixtureHandler(t *testing.T) {
	f := loadFixture()
	recorder := httptest.NewRecorder()
	f.handler(recorder, fixtureRequest(f, f.request))
	response := recorder.Result()
	defer response.Body.Close()
	body, _ := io.ReadAll(response.Body)
	if response.StatusCode != 200 || response.ContentLength != int64(len(f.response)) || !bytes.Equal(body, f.response) {
		t.Fatal("wrong fixture response")
	}
	for _, h := range f.ResponseHeaders {
		if response.Header.Get(h[0]) != h[1] {
			t.Fatal("wrong response header", h[0])
		}
	}
	for _, change := range []func(*http.Request){
		func(r *http.Request) { r.Method = "GET" },
		func(r *http.Request) { r.Header.Set("Authorization", "wrong") },
		func(r *http.Request) { r.ContentLength++ },
		func(r *http.Request) { r.Body = io.NopCloser(strings.NewReader(strings.Repeat("x", len(f.request)))) },
	} {
		r := fixtureRequest(f, f.request)
		change(r)
		recorder := httptest.NewRecorder()
		f.handler(recorder, r)
		if recorder.Code != 400 {
			t.Fatal("accepted invalid request")
		}
	}
}
func TestLoadValidationAndReuse(t *testing.T) {
	f := loadFixture()
	server := httptest.NewServer(http.HandlerFunc(f.handler))
	defer server.Close()
	report, err := load([]string{"--address", strings.TrimPrefix(server.URL, "http://"), "--concurrency", "2", "--warmup", "20ms", "--duration", "30ms"})
	if err != nil || !report.Valid || report.Successes == 0 || report.NewConnections != 2 || report.MeasuredNewConnections != 0 {
		t.Fatalf("invalid report: %+v %v", report, err)
	}
	bad := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { w.Write([]byte("corrupt")) }))
	defer bad.Close()
	report, err = load([]string{"--address", strings.TrimPrefix(bad.URL, "http://"), "--warmup", "1ms", "--duration", "1ms"})
	if err != nil || report.Valid || len(report.Errors) == 0 || report.RequestsPerSecond != nil {
		t.Fatal("corrupt response produced a rate")
	}
}
