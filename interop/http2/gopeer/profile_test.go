package main

import (
	"bytes"
	"io"
	"net"
	"reflect"
	"strings"
	"testing"
	"time"

	"golang.org/x/net/http2"
	"golang.org/x/net/http2/hpack"
)

func TestReferenceClientPreservesAllTrailerOccurrences(t *testing.T) {
	listener, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	if err := listener.(*net.TCPListener).SetDeadline(time.Now().Add(3 * time.Second)); err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() {
		done <- func() error {
			conn, err := listener.Accept()
			if err != nil {
				return err
			}
			defer conn.Close()
			p, err := newPeer(conn)
			if err != nil {
				return err
			}
			preface := make([]byte, len(http2.ClientPreface))
			if _, err := io.ReadFull(p.conn, preface); err != nil {
				return err
			}
			if err := p.framer.WriteSettings(); err != nil {
				return err
			}
			for {
				frame, err := p.read(true)
				if err != nil {
					return err
				}
				if _, ok := frame.(*http2.MetaHeadersFrame); ok {
					if err := p.headers(1, fields("200", 0), false, false); err != nil {
						return err
					}
					if err := p.headers(1, []hpack.HeaderField{
						{Name: "x-list", Value: "one"}, {Name: "x-other", Value: "marker"},
						{Name: "x-list", Value: "two"},
					}, true, false); err != nil {
						return err
					}
					for {
						_, err := p.read(true)
						if err == io.EOF {
							return nil
						}
						if err != nil {
							return err
						}
					}
				}
			}
		}()
	}()
	report, clientErr := rawClient(spec{
		Host: "127.0.0.1", Port: listener.Addr().(*net.TCPAddr).Port, RequestCount: 1,
		Requests: []request{{Method: "GET", Path: "/trailers"}},
	})
	if serverErr := <-done; clientErr != nil || serverErr != nil {
		t.Fatalf("client=%v server=%v", clientErr, serverErr)
	}
	expected := [][]string{{"x-list", "one"}, {"x-other", "marker"}, {"x-list", "two"}}
	if !reflect.DeepEqual(report["streams"].([]*result)[0].Trailers, expected) {
		t.Fatalf("trailer occurrences changed: %v", report)
	}
}

func TestWrapperWarmupAndCreditFaultControls(t *testing.T) {
	for _, mode := range []string{"valid", "early-headers", "excess-data", "excess-after-response", "bad-payload"} {
		t.Run(mode, func(t *testing.T) {
			listener, err := net.Listen("tcp4", "127.0.0.1:0")
			if err != nil {
				t.Fatal(err)
			}
			defer listener.Close()
			if err := listener.(*net.TCPListener).SetDeadline(time.Now().Add(3 * time.Second)); err != nil {
				t.Fatal(err)
			}
			type outcome struct {
				report map[string]any
				err    error
			}
			done := make(chan outcome, 1)
			go func() {
				conn, err := listener.Accept()
				var report map[string]any
				if err == nil {
					report, err = serve(conn, "early-response-wrapper")
				}
				done <- outcome{report, err}
			}()
			address := listener.Addr().(*net.TCPAddr)
			var clientErr error
			if mode == "valid" || mode == "early-headers" {
				concurrency := 2
				if mode == "early-headers" {
					concurrency = 4
				}
				_, clientErr = rawClient(spec{
					Host: "127.0.0.1", Port: address.Port, RequestCount: 4, Concurrency: concurrency,
					Requests: []request{
						{Method: "GET", Path: "/bytes/0"}, {Method: "GET", Path: "/bytes/0"},
						{Method: "POST", Path: "/protocol", BodyBytes: 131087}, {Method: "GET", Path: "/sibling"},
					},
				})
			} else {
				clientErr = func() error {
					conn, err := net.DialTimeout("tcp4", listener.Addr().String(), time.Second)
					if err != nil {
						return err
					}
					defer conn.Close()
					p, err := newPeer(conn)
					if err != nil {
						return err
					}
					if _, err := io.WriteString(p.conn, http2.ClientPreface); err != nil {
						return err
					}
					if err := p.framer.WriteSettings(); err != nil {
						return err
					}
					headers := []hpack.HeaderField{
						{Name: ":method", Value: "GET"}, {Name: ":scheme", Value: "http"},
						{Name: ":authority", Value: "localhost"}, {Name: ":path", Value: "/bytes/0"},
					}
					for _, stream := range []uint32{1, 3} {
						end := mode != "excess-after-response"
						if err := p.headers(stream, headers, end, false); err != nil {
							return err
						}
						if !end {
							if err := p.body(stream, nil, true); err != nil {
								return err
							}
						}
					}
					ended := 0
					for ended != 2 {
						frame, err := p.read(true)
						if err != nil {
							return err
						}
						if frame, ok := frame.(*http2.MetaHeadersFrame); ok && frame.StreamEnded() {
							ended++
						}
					}
					headers[0].Value = "POST"
					if err := p.headers(5, headers, false, false); err != nil {
						return err
					}
					// Intentional fault control bypasses the compliant p.body sender.
					data := make([]byte, 1025)
					if mode == "bad-payload" {
						data = make([]byte, 1024)
					}
					if mode == "excess-after-response" {
						data = bytes.Repeat([]byte{5}, 1024)
					}
					if err := p.framer.WriteData(5, false, data); err != nil {
						return err
					}
					if mode == "excess-after-response" {
						for {
							frame, err := p.read(true)
							if err != nil {
								return err
							}
							if frame, ok := frame.(*http2.MetaHeadersFrame); ok && frame.StreamID == 5 && frame.StreamEnded() {
								break
							}
						}
						if err := p.framer.WriteData(5, false, []byte{5}); err != nil {
							return err
						}
					}
					_, err = p.read(true)
					return err
				}()
			}
			server := <-done
			switch mode {
			case "valid":
				if clientErr != nil || server.err != nil || server.report["warmup_barrier"] != true {
					t.Fatalf("warmup failed: client=%v server=%v report=%v", clientErr, server.err, server.report)
				}
			case "early-headers":
				if server.err == nil || !strings.Contains(server.err.Error(), "HEADERS preceded wrapper warmup") {
					t.Fatalf("early HEADERS escaped: client=%v server=%v", clientErr, server.err)
				}
			case "excess-data", "excess-after-response":
				if server.err == nil || !strings.Contains(server.err.Error(), "exceeded post-barrier stream credit") {
					t.Fatalf("excess DATA escaped: client=%v server=%v", clientErr, server.err)
				}
			case "bad-payload":
				if server.err == nil || !strings.Contains(server.err.Error(), "upload payload mismatch") {
					t.Fatalf("bad payload escaped: client=%v server=%v", clientErr, server.err)
				}
			}
		})
	}
}
