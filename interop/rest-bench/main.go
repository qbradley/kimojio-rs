// One REST-shaped workload. Server and load generator are separate processes.
package main

import (
	"bytes"
	"embed"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"math"
	"math/bits"
	"net"
	"net/http"
	"os"
	"runtime"
	"strconv"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
)

//go:embed fixtures/*
var fixtures embed.FS

type manifest struct {
	Method          string      `json:"method"`
	Path            string      `json:"path"`
	RequestHeaders  [][2]string `json:"request_headers"`
	ResponseHeaders [][2]string `json:"response_headers"`
	RequestBytes    int         `json:"request_bytes"`
	ResponseBytes   int         `json:"response_bytes"`
}
type fixture struct {
	manifest
	request, response []byte
}

func loadFixture() fixture {
	read := func(name string) []byte {
		b, err := fixtures.ReadFile("fixtures/" + name)
		if err != nil {
			panic(err)
		}
		return b
	}
	f := fixture{request: read("request.json"), response: read("response.json")}
	if err := json.Unmarshal(read("manifest.json"), &f.manifest); err != nil {
		panic(err)
	}
	if !json.Valid(f.request) || !json.Valid(f.response) || len(f.request) != f.RequestBytes || len(f.response) != f.ResponseBytes {
		panic("invalid fixture")
	}
	return f
}
func (f fixture) handler(w http.ResponseWriter, r *http.Request) {
	valid := r.Method == f.Method && r.RequestURI == f.Path && r.ContentLength == int64(len(f.request)) && len(r.TransferEncoding) == 0
	for _, h := range f.RequestHeaders {
		got := r.Header.Get(h[0])
		if h[0] == "host" {
			got = r.Host
		}
		valid = valid && got == h[1]
	}
	if valid {
		body, err := io.ReadAll(io.LimitReader(r.Body, int64(len(f.request)+1)))
		valid = err == nil && bytes.Equal(body, f.request)
	}
	if !valid {
		http.Error(w, "bad request", http.StatusBadRequest)
		return
	}
	for _, h := range f.ResponseHeaders {
		w.Header().Set(h[0], h[1])
	}
	w.Header().Set("Content-Length", strconv.Itoa(len(f.response)))
	w.WriteHeader(http.StatusOK)
	_, _ = w.Write(f.response) // immutable shared bytes; no per-request JSON generation
}

type listener struct{ net.Listener }

func (l listener) Accept() (net.Conn, error) {
	c, err := l.Listener.Accept()
	if err != nil {
		return nil, err
	}
	tcp := c.(*net.TCPConn)
	if err = tcp.SetNoDelay(true); err == nil {
		err = tcp.SetKeepAlive(false)
	}
	if err != nil {
		c.Close()
		return nil, err
	}
	return c, nil
}
func serve(args []string) error {
	flags := flag.NewFlagSet("serve", flag.ContinueOnError)
	bind := flags.String("bind", "127.0.0.1:0", "listen address")
	if err := flags.Parse(args); err != nil {
		return err
	}
	if runtime.GOMAXPROCS(0) != 1 {
		return errors.New("server requires GOMAXPROCS=1")
	}
	f := loadFixture()
	ln, err := net.Listen("tcp4", *bind)
	if err != nil {
		return err
	}
	fmt.Printf("LISTEN %s\n", ln.Addr())
	server := &http.Server{Handler: http.HandlerFunc(f.handler), ReadHeaderTimeout: 5 * time.Second,
		IdleTimeout: 30 * time.Second, MaxHeaderBytes: 16 * 1024}
	return server.Serve(listener{ln})
}

type histogram [2048]uint64

func (h *histogram) add(d time.Duration) {
	n := uint64(max(d.Nanoseconds(), 1))
	exponent := bits.Len64(n) - 1
	base := uint64(1) << exponent
	h[exponent*32+int((n-base)*32/base)]++
}
func (h *histogram) quantile(q float64) float64 {
	var n uint64
	for _, v := range h {
		n += v
	}
	if n == 0 {
		return 0
	}
	wanted := uint64(math.Ceil(float64(n) * q))
	var sum uint64
	for i, v := range h {
		sum += v
		if sum >= wanted {
			base := math.Exp2(float64(i / 32))
			return (base + base*float64(i%32+1)/32) / 1000
		}
	}
	panic("histogram overflow")
}

type stats struct {
	Attempts, Successes uint64
	Histogram           histogram
	Errors              []string
}
type output struct {
	Valid                  bool      `json:"valid"`
	Concurrency            int       `json:"concurrency"`
	WarmupSeconds          float64   `json:"warmup_seconds"`
	MeasuredSeconds        float64   `json:"measured_seconds"`
	Attempts               uint64    `json:"attempts"`
	Successes              uint64    `json:"successes"`
	Errors                 []string  `json:"errors"`
	RequestsPerSecond      *float64  `json:"requests_per_second"`
	P50US                  float64   `json:"p50_us"`
	P95US                  float64   `json:"p95_us"`
	P99US                  float64   `json:"p99_us"`
	NewConnections         int64     `json:"new_connections_total"`
	MeasuredNewConnections int64     `json:"measured_new_connections"`
	WarmupSuccesses        uint64    `json:"warmup_successes"`
	CPUUserSeconds         float64   `json:"client_user_seconds"`
	CPUSystemSeconds       float64   `json:"client_system_seconds"`
	GOMAXPROCS             int       `json:"client_gomaxprocs"`
	StartUnixNS            int64     `json:"start_unix_ns"`
	EndUnixNS              int64     `json:"end_unix_ns"`
	Histogram              histogram `json:"latency_histogram"`
}

func load(args []string) (output, error) {
	flags := flag.NewFlagSet("load", flag.ContinueOnError)
	address := flags.String("address", "", "server host:port")
	concurrency := flags.Int("concurrency", 16, "1..32 prewarmed connections")
	warmup := flags.Duration("warmup", 2*time.Second, "warmup duration")
	duration := flags.Duration("duration", 10*time.Second, "measurement admission duration")
	if err := flags.Parse(args); err != nil {
		return output{}, err
	}
	host, _, err := net.SplitHostPort(*address)
	if err != nil || host != "127.0.0.1" || *concurrency < 1 || *concurrency > 32 || *duration <= 0 || *duration > time.Minute || *warmup <= 0 || *warmup > 30*time.Second {
		return output{}, errors.New("invalid load bounds")
	}
	f := loadFixture()
	var connections atomic.Int64
	wire := requestWire(f)
	ready := make(chan stats, *concurrency)
	done := make(chan stats, *concurrency)
	start := make(chan struct{})
	var until time.Time
	warmUntil := time.Now().Add(*warmup)
	var workers sync.WaitGroup
	for worker := 0; worker < *concurrency; worker++ {
		workers.Add(1)
		go func() {
			defer workers.Done()
			client, err := dialPeer(*address, &f, wire)
			warm := stats{}
			if err != nil {
				warm.Errors = append(warm.Errors, err.Error())
			} else {
				connections.Add(1)
				defer client.conn.Close()
			}
			for len(warm.Errors) == 0 && (warm.Successes == 0 || time.Now().Before(warmUntil)) {
				warm.Attempts++
				if err := client.exchange(); err != nil {
					warm.Errors = append(warm.Errors, err.Error())
					break
				}
				warm.Successes++
			}
			ready <- warm
			<-start
			local := stats{}
			if len(warm.Errors) == 0 {
				for time.Now().Before(until) {
					began := time.Now()
					local.Attempts++
					if err := client.exchange(); err != nil {
						local.Errors = append(local.Errors, err.Error())
						break
					}
					local.Successes++
					local.Histogram.add(time.Since(began))
				}
			}
			done <- local
		}()
	}
	result := output{Concurrency: *concurrency, WarmupSeconds: warmup.Seconds(), GOMAXPROCS: runtime.GOMAXPROCS(0)}
	for i := 0; i < *concurrency; i++ {
		s := <-ready
		result.WarmupSuccesses += s.Successes
		result.Errors = append(result.Errors, s.Errors...)
	}
	initialConnections := connections.Load()
	var before, after syscall.Rusage
	if err := syscall.Getrusage(syscall.RUSAGE_SELF, &before); err != nil {
		result.Errors = append(result.Errors, err.Error())
	}
	began := time.Now()
	until = began.Add(*duration)
	close(start)
	for i := 0; i < *concurrency; i++ {
		s := <-done
		result.Attempts += s.Attempts
		result.Successes += s.Successes
		result.Errors = append(result.Errors, s.Errors...)
		for j, n := range s.Histogram {
			result.Histogram[j] += n
		}
	}
	workers.Wait() // includes explicit idle connection cleanup
	ended := time.Now()
	if err := syscall.Getrusage(syscall.RUSAGE_SELF, &after); err != nil {
		result.Errors = append(result.Errors, err.Error())
	}
	result.StartUnixNS = began.UnixNano()
	result.EndUnixNS = ended.UnixNano()
	result.MeasuredSeconds = ended.Sub(began).Seconds()
	result.CPUUserSeconds = float64(after.Utime.Nano()-before.Utime.Nano()) / 1e9
	result.CPUSystemSeconds = float64(after.Stime.Nano()-before.Stime.Nano()) / 1e9
	result.NewConnections = connections.Load()
	result.MeasuredNewConnections = result.NewConnections - initialConnections
	if initialConnections != int64(*concurrency) || result.MeasuredNewConnections != 0 {
		result.Errors = append(result.Errors, "expected exactly one reused TCP connection per worker")
	}
	result.Valid = len(result.Errors) == 0 && result.Successes > 0 && result.Attempts == result.Successes
	result.P50US = result.Histogram.quantile(.5)
	result.P95US = result.Histogram.quantile(.95)
	result.P99US = result.Histogram.quantile(.99)
	if result.Valid {
		rate := float64(result.Successes) / result.MeasuredSeconds
		result.RequestsPerSecond = &rate
	}
	return result, nil
}
func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "expected serve or load")
		os.Exit(1)
	}
	var err error
	switch os.Args[1] {
	case "serve":
		err = serve(os.Args[2:])
	case "load":
		var r output
		r, err = load(os.Args[2:])
		if err == nil {
			err = json.NewEncoder(os.Stdout).Encode(r)
			if !r.Valid {
				os.Exit(1)
			}
		}
	default:
		err = errors.New("expected serve or load")
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
