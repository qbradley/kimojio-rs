package main

import (
	"bytes"
	"fmt"
	"io"
	"net"
	"testing"
	"time"

	"golang.org/x/net/http2"
	"golang.org/x/net/http2/hpack"
)

func TestPushHeadersMustAdvanceSharedHPACKHistory(t *testing.T) {
	var buffer bytes.Buffer
	encoder := hpack.NewEncoder(&buffer)
	if err := encoder.WriteField(hpack.HeaderField{Name: "x-sync", Value: "promised-dynamic-table-value"}); err != nil {
		t.Fatal(err)
	}
	promise := append([]byte(nil), buffer.Bytes()...)
	buffer.Reset()
	if err := encoder.WriteField(hpack.HeaderField{Name: "x-sync", Value: "promised-dynamic-table-value"}); err != nil {
		t.Fatal(err)
	}
	response := append([]byte(nil), buffer.Bytes()...)
	if _, err := hpack.NewDecoder(4096, nil).DecodeFull(response); err == nil {
		t.Fatal("negative control: response decoded without promised history")
	}
	decoder := hpack.NewDecoder(4096, nil)
	if _, err := decoder.DecodeFull(promise); err != nil {
		t.Fatal(err)
	}
	fields, err := decoder.DecodeFull(response)
	if err != nil || len(fields) != 1 || fields[0].Value != "promised-dynamic-table-value" {
		t.Fatalf("shared HPACK state failed: fields=%v, error=%v", fields, err)
	}
}

func TestBodyRespectsCumulativeStreamAndConnectionCredit(t *testing.T) {
	var output bytes.Buffer
	p := &peer{
		framer: http2.NewFramer(&output, nil), window: 10, credit: 20,
		updates: make(map[uint32]uint64), streamSent: make(map[uint32]int64),
	}
	if err := p.body(1, make([]byte, 7), false); err != nil {
		t.Fatal(err)
	}
	if err := p.body(1, make([]byte, 4), false); err == nil {
		t.Fatal("negative control: exceeded cumulative stream credit")
	}
	p.updates[1] = 4
	if err := p.body(1, make([]byte, 4), false); err != nil {
		t.Fatal(err)
	}
	if err := p.body(3, make([]byte, 10), false); err == nil {
		t.Fatal("negative control: exceeded connection credit")
	}
}

func TestClassicConnectUsesIndependentSocketPeer(t *testing.T) {
	listener, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	if err := listener.(*net.TCPListener).SetDeadline(time.Now().Add(10 * time.Second)); err != nil {
		t.Fatal(err)
	}
	completed := make(chan error, 1)
	go func() {
		conn, err := listener.Accept()
		if err == nil {
			_, err = serve(conn, "connect")
		}
		completed <- err
	}()
	address := listener.Addr().(*net.TCPAddr)
	output, clientErr := rawClient(spec{
		Host: "127.0.0.1", Port: address.Port, RequestCount: 1,
		Requests: []request{{Method: "CONNECT", BodyBytes: 37}},
	})
	serverErr := <-completed
	if clientErr != nil || serverErr != nil {
		t.Fatalf("client=%v server=%v", clientErr, serverErr)
	}
	results := output["streams"].([]*result)
	if len(results) != 1 || results[0].Bytes != 37 || !results[0].Ended || results[0].Outcome != "complete" {
		t.Fatalf("bad result: %v", results)
	}
}

func TestFramerRejectsInterleavedContinuation(t *testing.T) {
	var buffer bytes.Buffer
	writer := http2.NewFramer(&buffer, nil)
	if err := writer.WriteHeaders(http2.HeadersFrameParam{StreamID: 1, BlockFragment: []byte{0x88}}); err != nil {
		t.Fatal(err)
	}
	if err := writer.WritePing(false, [8]byte{}); err != nil {
		t.Fatal(err)
	}
	reader := http2.NewFramer(io.Discard, &buffer)
	reader.ReadMetaHeaders = hpack.NewDecoder(4096, nil)
	if _, err := reader.ReadFrame(); err != http2.ConnectionError(http2.ErrCodeProtocol) {
		t.Fatalf("negative control: expected PROTOCOL_ERROR, got %v", err)
	}
}

func TestFinalResponseWithoutActionDoesNotStopUpload(t *testing.T) {
	for _, status := range []string{"200", "413"} {
		t.Run(status, func(t *testing.T) {
			listener, err := net.Listen("tcp4", "127.0.0.1:0")
			if err != nil {
				t.Fatal(err)
			}
			defer listener.Close()
			if err := listener.(*net.TCPListener).SetDeadline(time.Now().Add(10 * time.Second)); err != nil {
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
					if string(preface) != http2.ClientPreface {
						return fmt.Errorf("bad preface")
					}
					if err := p.framer.WriteSettings(); err != nil {
						return err
					}
					received, refunded := 0, 0
					for {
						frame, err := p.read(true)
						if err != nil {
							return err
						}
						switch frame := frame.(type) {
						case *http2.MetaHeadersFrame:
							if err := p.headers(1, fields(status, 0), true, false); err != nil {
								return err
							}
						case *http2.DataFrame:
							received += len(frame.Data())
							if !bytes.Equal(frame.Data(), bytes.Repeat([]byte{1}, len(frame.Data()))) {
								return fmt.Errorf("wrong upload bytes")
							}
							if frame.StreamEnded() {
								if received != 131087 {
									return fmt.Errorf("upload stopped at %d", received)
								}
								return nil
							}
							// Surplus credit can race the client's final socket close.
							grant := min(int(frame.Length), 131087-65535-refunded)
							if grant > 0 {
								if err := p.framer.WriteWindowUpdate(0, uint32(grant)); err != nil {
									return err
								}
								if err := p.framer.WriteWindowUpdate(1, uint32(grant)); err != nil {
									return err
								}
								refunded += grant
							}
						case *http2.RSTStreamFrame:
							return fmt.Errorf("unrequested upload reset: %v", frame.ErrCode)
						}
					}
				}()
			}()
			address := listener.Addr().(*net.TCPAddr)
			report, clientErr := rawClient(spec{
				Host: "127.0.0.1", Port: address.Port, RequestCount: 1,
				Requests: []request{{Method: "POST", Path: "/early-final", BodyBytes: 131087}},
			})
			if clientErr != nil {
				listener.Close()
			}
			serverErr := <-done
			if clientErr != nil || serverErr != nil {
				t.Fatalf("client=%v server=%v", clientErr, serverErr)
			}
			r := report["streams"].([]*result)[0]
			if !r.Ended || r.Bytes != 0 || r.Outcome != "complete" || r.Error != nil {
				t.Fatalf("early response changed upload retirement: %+v", r)
			}
		})
	}
}
