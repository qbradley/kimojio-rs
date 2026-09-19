// Independent HTTP/1 peers. This file uses only the Go standard library.
package main

import (
	"bytes"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"net/http/httptrace"
	"net/textproto"
	"net/url"
	"os"
	"strings"
	"sync"
	"time"
)

type boundedListener struct {
	net.Listener
	slots  chan struct{}
	closed chan struct{}
	once   sync.Once
}

func (l *boundedListener) Accept() (net.Conn, error) {
	select {
	case l.slots <- struct{}{}:
	case <-l.closed:
		return nil, net.ErrClosed
	}
	connection, err := l.Listener.Accept()
	if err != nil {
		<-l.slots
		return nil, err
	}
	return &boundedConnection{Conn: connection, release: func() { <-l.slots }}, nil
}

func (l *boundedListener) Close() error {
	l.once.Do(func() { close(l.closed) })
	return l.Listener.Close()
}

type boundedConnection struct {
	net.Conn
	release func()
	once    sync.Once
}

func (c *boundedConnection) Close() error {
	err := c.Conn.Close()
	c.once.Do(c.release)
	return err
}

func main() {
	mode := flag.String("mode", "server", "server or client")
	bind := flag.String("bind", "127.0.0.1:0", "listen address")
	root := flag.String("root", ".", "static-file directory")
	url := flag.String("url", "", "client URL")
	method := flag.String("method", "GET", "client method")
	body := flag.String("body", "", "client request body")
	bodyFile := flag.String("body-file", "", "binary client request body file")
	count := flag.Int("count", 2, "sequential client requests")
	chunked := flag.Bool("chunked", false, "send an unknown-length request body")
	expect := flag.Bool("expect-continue", false, "gate upload on 100 Continue")
	maxConnections := flag.Int("max-connections", 0, "maximum active server connections; zero disables the limit")
	flag.Parse()
	if *mode == "client" {
		if *bodyFile != "" {
			data, err := os.ReadFile(*bodyFile)
			if err != nil {
				log.Fatal(err)
			}
			*body = string(data)
		}
		if err := client(*url, *method, *body, *count, *chunked, *expect); err != nil {
			log.Fatal(err)
		}
		return
	}
	if *mode != "server" {
		log.Fatalf("unknown mode %q", *mode)
	}
	if *maxConnections < 0 || *maxConnections > 256 {
		log.Fatal("max-connections must be between 0 and 256")
	}
	host, _, err := net.SplitHostPort(*bind)
	if err != nil || host != "127.0.0.1" {
		log.Fatal("server requires an IPv4 loopback bind address")
	}
	listener, err := net.Listen("tcp4", *bind)
	if err != nil {
		log.Fatal(err)
	}
	if *maxConnections > 0 {
		listener = &boundedListener{
			Listener: listener,
			slots:    make(chan struct{}, *maxConnections),
			closed:   make(chan struct{}),
		}
	}
	defer listener.Close()
	files := http.FileServer(http.Dir(*root))
	handler := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/__peer/duplex":
			controller := http.NewResponseController(w)
			if err := controller.EnableFullDuplex(); err != nil {
				http.Error(w, err.Error(), http.StatusInternalServerError)
				return
			}
			r.Body = http.MaxBytesReader(w, r.Body, 8<<20)
			defer r.Body.Close()
			if r.ProtoAtLeast(1, 1) && strings.EqualFold(r.Header.Get("Expect"), "100-continue") {
				w.WriteHeader(http.StatusContinue)
			}
			w.Header().Set("Trailer", "X-Duplex")
			w.WriteHeader(http.StatusOK)
			if err := controller.Flush(); err != nil {
				return
			}
			var buffer [3]byte
			for {
				n, err := r.Body.Read(buffer[:])
				if n > 0 {
					if _, writeErr := w.Write(buffer[:n]); writeErr != nil {
						return
					}
					if flushErr := controller.Flush(); flushErr != nil {
						return
					}
				}
				if err == io.EOF {
					break
				}
				if err != nil {
					return
				}
			}
			for _, value := range r.Trailer.Values("X-Duplex") {
				w.Header().Add("X-Duplex", value)
			}
		case "/__peer/echo":
			defer r.Body.Close()
			data, err := io.ReadAll(http.MaxBytesReader(w, r.Body, 8<<20))
			if err != nil {
				http.Error(w, "invalid or oversized body", http.StatusBadRequest)
				return
			}
			w.Header().Set("Content-Type", "application/octet-stream")
			w.Header().Set("Content-Length", fmt.Sprint(len(data)))
			w.WriteHeader(http.StatusOK)
			_, _ = w.Write(data)
		case "/__peer/chunked":
			w.Header().Set("Trailer", "X-Peer-End")
			w.WriteHeader(http.StatusOK)
			for _, part := range []string{"one", "two", "three"} {
				_, _ = io.WriteString(w, part)
				w.(http.Flusher).Flush()
			}
			w.Header().Set("X-Peer-End", "done")
		case "/__peer/reject":
			w.Header().Set("Connection", "close")
			http.Error(w, "rejected", http.StatusExpectationFailed)
		case "/__peer/no-content":
			w.WriteHeader(http.StatusNoContent)
		default:
			files.ServeHTTP(w, r)
		}
	})
	server := &http.Server{
		Handler:           handler,
		ReadHeaderTimeout: 3 * time.Second,
		ReadTimeout:       5 * time.Second,
		WriteTimeout:      5 * time.Second,
		IdleTimeout:       3 * time.Second,
		MaxHeaderBytes:    64 << 10,
	}
	fmt.Printf("LISTEN %s\n", listener.Addr())
	if err := server.Serve(listener); err != nil && err != http.ErrServerClosed {
		log.Fatal(err)
	}
}

func client(address, method, body string, count int, chunked, expect bool) error {
	parsed, err := url.Parse(address)
	if err != nil || parsed.Scheme != "http" || parsed.Hostname() != "127.0.0.1" {
		return fmt.Errorf("client requires an http://127.0.0.1 URL")
	}
	if count < 1 || count > 100 {
		return fmt.Errorf("client requires --url and --count between 1 and 100")
	}
	transport := &http.Transport{
		Proxy:                 nil,
		DisableCompression:    true,
		MaxConnsPerHost:       1,
		MaxIdleConnsPerHost:   1,
		ResponseHeaderTimeout: 3 * time.Second,
		ExpectContinueTimeout: time.Second,
		ForceAttemptHTTP2:     false,
	}
	defer transport.CloseIdleConnections()
	client := &http.Client{
		Transport: transport,
		Timeout:   5 * time.Second,
		CheckRedirect: func(_ *http.Request, _ []*http.Request) error {
			return http.ErrUseLastResponse
		},
	}
	encoder := json.NewEncoder(os.Stdout)
	for i := 0; i < count; i++ {
		req, err := http.NewRequest(method, address, bytes.NewBufferString(body))
		if err != nil {
			return err
		}
		if chunked {
			req.ContentLength = -1
		}
		if expect {
			req.Header.Set("Expect", "100-continue")
		}
		interim := []int{}
		trace := &httptrace.ClientTrace{
			Got1xxResponse: func(code int, _ textproto.MIMEHeader) error {
				interim = append(interim, code)
				return nil
			},
		}
		req = req.WithContext(httptrace.WithClientTrace(req.Context(), trace))
		res, err := client.Do(req)
		if err != nil {
			return err
		}
		data, err := io.ReadAll(io.LimitReader(res.Body, (8<<20)+1))
		closeErr := res.Body.Close()
		if err != nil {
			return err
		}
		if closeErr != nil {
			return closeErr
		}
		if len(data) > 8<<20 {
			return fmt.Errorf("response exceeds 8 MiB")
		}
		if err := encoder.Encode(struct {
			Index    int         `json:"index"`
			Status   int         `json:"status"`
			Headers  http.Header `json:"headers"`
			Body     []byte      `json:"body_base64"`
			Interim  []int       `json:"interim_statuses"`
			Trailers http.Header `json:"trailers"`
		}{i, res.StatusCode, res.Header, data, interim, res.Trailer}); err != nil {
			return err
		}
	}
	return nil
}
