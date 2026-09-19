package main

import (
	"net/http"
	"net/http/httptest"
	"sync/atomic"
	"syscall"
	"testing"
	"time"
)

func TestMeasurementUsesBaselineDeltaAndActualWindow(t *testing.T) {
	start := time.Unix(1000, 0)
	window := measurement{started: start, usage: syscall.Rusage{
		Utime: syscall.Timeval{Sec: 7, Usec: 100000},
		Stime: syscall.Timeval{Sec: 3, Usec: 200000},
	}}
	end := syscall.Rusage{
		Utime: syscall.Timeval{Sec: 7, Usec: 120000},
		Stime: syscall.Timeval{Sec: 3, Usec: 207000},
	}
	var result report
	recordMeasurement(&result, window, end, start.Add(50*time.Millisecond), nil)
	if result.CPUUserSeconds != .020 || result.CPUSystemSeconds != .007 || result.ElapsedSeconds != .050 {
		t.Fatalf("inconsistent measurement: %+v", result)
	}
	if result.MeasurementStartUnixNS != start.UnixNano() ||
		result.MeasurementEndUnixNS != start.Add(50*time.Millisecond).UnixNano() {
		t.Fatal("window timestamps do not match actual boundaries")
	}
}

func TestMeasurementRejectsNegativeCPUWindow(t *testing.T) {
	window := measurement{started: time.Now(), usage: syscall.Rusage{Utime: syscall.Timeval{Sec: 1}}}
	var result report
	recordMeasurement(&result, window, syscall.Rusage{}, window.started.Add(time.Millisecond), nil)
	if len(result.Errors) == 0 {
		t.Fatal("negative CPU delta accepted")
	}
}

func TestHTTPMeasurementIncludesAdmittedRequestDrain(t *testing.T) {
	var requests atomic.Int32
	var observedStart atomic.Int64
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		requests.Add(1)
		observedStart.Store(time.Now().UnixNano())
		time.Sleep(80 * time.Millisecond)
		w.Header().Set("Content-Length", "3")
		w.Write([]byte("xxx"))
	}))
	defer server.Close()
	result, err := loadHTTP([]string{"--url", server.URL, "--size", "3", "--duration", "40ms"})
	if err != nil || len(result.Errors) != 0 || result.Successes != 1 || requests.Load() != 1 {
		t.Fatalf("drain failed: count=%d requests=%d errors=%v err=%v", result.Successes, requests.Load(), result.Errors, err)
	}
	if observedStart.Load() < result.MeasurementStartUnixNS || result.ElapsedSeconds < .080 {
		t.Fatalf("nominal rather than actual measurement window: %f", result.ElapsedSeconds)
	}
}
