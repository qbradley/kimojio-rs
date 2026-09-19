package main

import (
	"bytes"
	"context"
	"encoding/binary"
	"errors"
	"flag"
	"fmt"
	"io"
	"math"
	"math/bits"
	"net"
	"net/http"
	"net/http/httptrace"
	"net/url"
	"sync"
	"sync/atomic"
	"time"

	"github.com/gorilla/websocket"
)

type histogram [2048]uint64

func (h *histogram) add(d time.Duration) {
	n := uint64(max(int64(d), 1))
	exponent := bits.Len64(n) - 1
	base := uint64(1) << exponent
	sub := (n - base) * 32 / base
	h[exponent*32+int(sub)]++
}

func (h *histogram) quantile(q float64) float64 {
	var count uint64
	for _, n := range h {
		count += n
	}
	if count == 0 {
		return 0
	}
	threshold := uint64(math.Ceil(float64(count) * q))
	var seen uint64
	for i, n := range h {
		seen += n
		if seen >= threshold {
			base := math.Exp2(float64(i / 32))
			return (base + base*float64(i%32+1)/32) / 1e6
		}
	}
	panic("histogram count mismatch")
}

type report struct {
	Schema                  int       `json:"schema"`
	Mode                    string    `json:"mode"`
	Valid                   bool      `json:"valid"`
	Errors                  []string  `json:"errors"`
	Attempts                uint64    `json:"attempts"`
	Successes               uint64    `json:"successes"`
	Deliveries              uint64    `json:"deliveries"`
	Bytes                   uint64    `json:"validated_payload_bytes"`
	ElapsedSeconds          float64   `json:"elapsed_seconds"`
	InvocationSeconds       float64   `json:"invocation_seconds"`
	SuccessesPerSecond      *float64  `json:"successes_per_second"`
	P50MS                   float64   `json:"p50_ms"`
	P95MS                   float64   `json:"p95_ms"`
	P99MS                   float64   `json:"p99_ms"`
	Histogram               histogram `json:"latency_histogram"`
	CPUUserSeconds          float64   `json:"cpu_user_seconds"`
	CPUSystemSeconds        float64   `json:"cpu_system_seconds"`
	ProcessCPUUserSeconds   float64   `json:"process_cpu_user_seconds"`
	ProcessCPUSystemSeconds float64   `json:"process_cpu_system_seconds"`
	MeasurementScope        string    `json:"measurement_scope"`
	MeasurementStartUnixNS  int64     `json:"measurement_start_unix_ns"`
	MeasurementEndUnixNS    int64     `json:"measurement_end_unix_ns"`
	MaxRSSKiB               int64     `json:"max_rss_kib"`
	VoluntarySwitches       int64     `json:"voluntary_context_switches"`
	InvoluntarySwitches     int64     `json:"involuntary_context_switches"`
	NewConnections          int64     `json:"new_connections"`
	ReusedConnections       int64     `json:"reused_connections"`
	IntentionalCloses       int       `json:"intentional_normal_closes"`
	GOMAXPROCS              int       `json:"gomaxprocs"`
	GoVersion               string    `json:"go_version"`
	FinalFDs                int       `json:"final_fds"`
	FinalHeapAllocBytes     uint64    `json:"final_go_heap_alloc_bytes"`
	FinalHeapInuseBytes     uint64    `json:"final_go_heap_inuse_bytes"`
	TotalAllocatedBytes     uint64    `json:"total_go_allocated_bytes"`
	Configuration           any       `json:"configuration"`
}

type loadFlags struct {
	URL         string
	Duration    time.Duration
	Timeout     time.Duration
	Size        int
	Concurrency int
	Fanout      int
	Fresh       bool
	NoDelay     bool
	Framing     string
}

func parseLoad(args []string, ws bool) (loadFlags, error) {
	var config loadFlags
	f := flag.NewFlagSet("load", flag.ContinueOnError)
	f.StringVar(&config.URL, "url", "", "loopback URL")
	f.DurationVar(&config.Duration, "duration", 2*time.Second, "admission duration, at most30s")
	f.DurationVar(&config.Timeout, "timeout", 5*time.Second, "per-operation deadline")
	f.IntVar(&config.Size, "size", 128, "exact expected bytes")
	f.IntVar(&config.Concurrency, "concurrency", 1, "HTTP workers")
	f.IntVar(&config.Fanout, "fanout", 1, "WS recipients INCLUDING publisher")
	f.BoolVar(&config.Fresh, "fresh", false, "one HTTP exchange per TCP connection")
	f.BoolVar(&config.NoDelay, "nodelay", true, "client TCP_NODELAY")
	f.StringVar(&config.Framing, "framing", "fixed", "fixed, trailers, or chunked-echo")
	if err := f.Parse(args); err != nil {
		return config, err
	}
	u, err := url.Parse(config.URL)
	scheme := "http"
	if ws {
		scheme = "ws"
	}
	if err != nil || u.Scheme != scheme || u.Hostname() != "127.0.0.1" || u.User != nil ||
		config.Duration <= 0 || config.Duration > 30*time.Second || config.Timeout <= 0 ||
		config.Timeout > 15*time.Second || config.Size < 0 || config.Size > 1<<20 ||
		config.Concurrency < 1 || config.Concurrency > 64 || config.Fanout < 1 || config.Fanout > 16 {
		return config, errors.New("invalid loopback URL or bounded load configuration")
	}
	if ws && (config.Size < 8 || config.Fresh) {
		return config, errors.New("WS requires at least8bytes and persistent connections")
	}
	if config.Framing != "fixed" && config.Framing != "trailers" && config.Framing != "chunked-echo" {
		return config, errors.New("unknown framing")
	}
	if config.Framing == "trailers" && config.Size != 13 {
		return config, errors.New("trailers fixture is exactly13bytes")
	}
	if config.Framing == "chunked-echo" && (ws || !config.Fresh || config.Size == 0) {
		return config, errors.New("chunked echo requires nonempty HTTP upload and fresh connections")
	}
	return config, nil
}

func dialer(config loadFlags) func(context.Context, string, string) (net.Conn, error) {
	return func(ctx context.Context, network, address string) (net.Conn, error) {
		conn, err := (&net.Dialer{Timeout: config.Timeout}).DialContext(ctx, network, address)
		if err != nil {
			return nil, err
		}
		if err := conn.(*net.TCPConn).SetNoDelay(config.NoDelay); err != nil {
			conn.Close()
			return nil, err
		}
		return conn, nil
	}
}

func loadHTTP(args []string) (result report, err error) {
	config, err := parseLoad(args, false)
	result = report{Mode: "http", Configuration: config}
	if err != nil {
		return result, err
	}
	expected := bytes.Repeat([]byte{'x'}, config.Size)
	if config.Framing == "trailers" {
		expected = []byte("first\nsecond\n")
	}
	transport := &http.Transport{
		DialContext: dialer(config), DisableCompression: true,
		MaxIdleConns: config.Concurrency, MaxIdleConnsPerHost: config.Concurrency,
		MaxConnsPerHost: config.Concurrency, DisableKeepAlives: config.Fresh,
		ResponseHeaderTimeout: config.Timeout, MaxResponseHeaderBytes: 65536,
	}
	client := &http.Client{Transport: transport, Timeout: config.Timeout,
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	var newConnections, reused atomic.Int64
	trace := &httptrace.ClientTrace{GotConn: func(info httptrace.GotConnInfo) {
		if info.Reused {
			reused.Add(1)
		} else {
			newConnections.Add(1)
		}
	}}
	outcomes := make(chan report, config.Concurrency)
	window, err := beginMeasurement()
	if err != nil {
		return result, err
	}
	defer finishMeasurement(&result, window)
	defer transport.CloseIdleConnections()
	until := window.started.Add(config.Duration)
	var workers sync.WaitGroup
	for worker := 0; worker < config.Concurrency; worker++ {
		workers.Add(1)
		go func(worker int) {
			defer workers.Done()
			local := report{}
			for {
				start := time.Now()
				if !start.Before(until) {
					break
				}
				local.Attempts++
				method := "GET"
				var source io.Reader
				if config.Framing == "chunked-echo" {
					method = "POST"
					source = bytes.NewReader(expected)
				}
				req, _ := http.NewRequestWithContext(httptrace.WithClientTrace(context.Background(), trace), method, config.URL, source)
				if config.Framing == "chunked-echo" {
					req.ContentLength = -1
					req.GetBody = nil
				}
				response, requestErr := client.Do(req)
				if requestErr == nil {
					body, readErr := io.ReadAll(io.LimitReader(response.Body, int64(config.Size)+1))
					response.Body.Close()
					requestErr = readErr
					if requestErr == nil && (response.StatusCode != 200 || !bytes.Equal(body, expected)) {
						requestErr = fmt.Errorf("wrong status/body: status%d bytes%d", response.StatusCode, len(body))
					}
					if requestErr == nil && config.Framing == "fixed" &&
						(response.ContentLength != int64(config.Size) || len(response.TransferEncoding) != 0) {
						requestErr = errors.New("expected exact Content-Length framing")
					}
					if requestErr == nil && config.Framing == "trailers" &&
						(len(response.TransferEncoding) != 1 || response.TransferEncoding[0] != "chunked" ||
							response.Trailer.Get("X-Finished") != "yes") {
						requestErr = errors.New("expected real chunked transfer and X-Finished:yes trailer")
					}
					if requestErr == nil && config.Framing == "chunked-echo" &&
						(len(response.TransferEncoding) != 1 || response.TransferEncoding[0] != "chunked" ||
							response.ContentLength != -1 || len(response.Trailer) != 0 || !response.Close) {
						requestErr = errors.New("expected streamed chunked echo, no trailers, and Connection:close")
					}
				}
				if requestErr != nil {
					local.Errors = append(local.Errors, fmt.Sprintf("worker%d attempt%d: %v", worker, local.Attempts, requestErr))
					break
				}
				local.Successes++
				local.Bytes += uint64(len(expected))
				local.Histogram.add(time.Since(start))
			}
			outcomes <- local
		}(worker)
	}
	for worker := 0; worker < config.Concurrency; worker++ {
		local := <-outcomes
		result.Attempts += local.Attempts
		result.Successes += local.Successes
		result.Bytes += local.Bytes
		result.Errors = append(result.Errors, local.Errors...)
		for i, count := range local.Histogram {
			result.Histogram[i] += count
		}
	}
	workers.Wait()
	result.NewConnections, result.ReusedConnections = newConnections.Load(), reused.Load()
	return result, nil
}

func payload(sequence uint64, size int) []byte {
	body := bytes.Repeat([]byte{0xa5}, size)
	binary.BigEndian.PutUint64(body, sequence)
	return body
}

func loadWS(args []string) (result report, err error) {
	config, err := parseLoad(args, true)
	result = report{Mode: "ws", Configuration: config}
	if err != nil {
		return result, err
	}
	dial := websocket.Dialer{NetDialContext: dialer(config), HandshakeTimeout: config.Timeout,
		EnableCompression: false, ReadBufferSize: 16384, WriteBufferSize: 16384}
	connections := make([]*websocket.Conn, 0, config.Fanout)
	var readers sync.WaitGroup
	stopReaders := make(chan struct{})
	var window *measurement
	defer func() {
		close(stopReaders)
		for _, conn := range connections {
			conn.Close()
		}
		readers.Wait()
		if window != nil {
			finishMeasurement(&result, *window)
		}
	}()
	for i := 0; i < config.Fanout; i++ {
		conn, _, err := dial.Dial(config.URL, nil)
		if err != nil {
			return result, err
		}
		conn.SetReadLimit(int64(config.Size))
		connections = append(connections, conn)
	}
	type receipt struct {
		recipient int
		sequence  uint64
		err       error
	}
	receipts := make(chan receipt, config.Fanout)
	closed := make(chan error, config.Fanout)
	registered := make(chan int, config.Fanout)
	var finishing atomic.Bool
	for recipient, conn := range connections {
		recipient := recipient
		conn.SetPongHandler(func(string) error {
			select {
			case registered <- recipient:
			default:
			}
			return nil
		})
		readers.Add(1)
		go func(recipient int, conn *websocket.Conn) {
			defer readers.Done()
			for sequence := uint64(0); ; sequence++ {
				conn.SetReadDeadline(time.Now().Add(config.Timeout))
				kind, body, err := conn.ReadMessage()
				if err != nil {
					if finishing.Load() {
						if websocket.IsCloseError(err, websocket.CloseNormalClosure) {
							err = nil
						}
						closed <- err
					} else {
						select {
						case receipts <- receipt{recipient: recipient, err: err}:
						case <-stopReaders:
						}
					}
					return
				}
				if kind != websocket.BinaryMessage || !bytes.Equal(body, payload(sequence, config.Size)) {
					err = fmt.Errorf("recipient%d sequence%d payload/order mismatch", recipient, sequence)
				}
				select {
				case receipts <- receipt{recipient, sequence, err}:
				case <-stopReaders:
					return
				}
				if err != nil {
					return
				}
			}
		}(recipient, conn)
	}
	for _, conn := range connections {
		if err := conn.WriteControl(websocket.PingMessage, []byte("ready"), time.Now().Add(config.Timeout)); err != nil {
			return result, err
		}
	}
	registrationDeadline := time.NewTimer(config.Timeout)
	defer registrationDeadline.Stop()
	ready := make([]bool, config.Fanout)
	for count := 0; count < config.Fanout; count++ {
		select {
		case recipient := <-registered:
			if ready[recipient] {
				return result, errors.New("duplicate registration pong")
			}
			ready[recipient] = true
		case failure := <-receipts:
			return result, fmt.Errorf("registration: %v", failure.err)
		case <-registrationDeadline.C:
			return result, errors.New("registration deadline")
		}
	}
	started, err := beginMeasurement()
	if err != nil {
		return result, err
	}
	window = &started
	until := window.started.Add(config.Duration)
	for len(result.Errors) == 0 {
		start := time.Now()
		if !start.Before(until) {
			break
		}
		sequence := result.Successes
		result.Attempts++
		connections[0].SetWriteDeadline(start.Add(config.Timeout))
		if err := connections[0].WriteMessage(websocket.BinaryMessage, payload(sequence, config.Size)); err != nil {
			result.Errors = append(result.Errors, err.Error())
			break
		}
		seen := make([]bool, config.Fanout)
		for count := 0; count < config.Fanout; count++ {
			value := <-receipts
			if value.err != nil {
				result.Errors = append(result.Errors, fmt.Sprintf("recipient%d: %v", value.recipient, value.err))
			} else if seen[value.recipient] || value.sequence != sequence {
				result.Errors = append(result.Errors, "duplicate/out-of-order recipient receipt")
			}
			seen[value.recipient] = true
		}
		if len(result.Errors) == 0 {
			result.Successes++
			result.Deliveries += uint64(config.Fanout)
			result.Bytes += uint64(config.Fanout * config.Size)
			result.Histogram.add(time.Since(start))
		}
	}
	finishing.Store(true)
	if len(result.Errors) == 0 {
		for _, conn := range connections {
			if err := conn.WriteControl(websocket.CloseMessage, websocket.FormatCloseMessage(1000, "done"), time.Now().Add(config.Timeout)); err != nil {
				result.Errors = append(result.Errors, err.Error())
			}
		}
		for range connections {
			select {
			case err := <-closed:
				if err != nil {
					result.Errors = append(result.Errors, fmt.Sprintf("close: %v", err))
				} else {
					result.IntentionalCloses++
				}
			case value := <-receipts:
				result.Errors = append(result.Errors, fmt.Sprintf("unexpected receipt during close: %v", value.err))
			case <-time.After(config.Timeout):
				result.Errors = append(result.Errors, "close deadline")
			}
		}
	} else {
		for _, conn := range connections {
			conn.Close()
		}
	}
	result.NewConnections = int64(config.Fanout)
	return result, nil
}
