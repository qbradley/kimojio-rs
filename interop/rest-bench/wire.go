package main

import (
	"bufio"
	"bytes"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"time"
)

func requestWire(f fixture) []byte {
	var wire bytes.Buffer
	fmt.Fprintf(&wire, "%s %s HTTP/1.1\r\n", f.Method, f.Path)
	for _, h := range f.RequestHeaders {
		fmt.Fprintf(&wire, "%s: %s\r\n", h[0], h[1])
	}
	fmt.Fprintf(&wire, "content-length: %d\r\n\r\n", len(f.request))
	wire.Write(f.request)
	return wire.Bytes()
}

type peer struct {
	conn    net.Conn
	input   *io.LimitedReader
	reader  *bufio.Reader
	body    []byte
	request []byte
	fixture *fixture
}

func dialPeer(address string, f *fixture, wire []byte) (*peer, error) {
	c, err := (&net.Dialer{Timeout: 5 * time.Second, KeepAlive: -1}).Dial("tcp4", address)
	if err != nil {
		return nil, err
	}
	if err = c.(*net.TCPConn).SetNoDelay(true); err != nil {
		c.Close()
		return nil, err
	}
	input := &io.LimitedReader{R: c}
	return &peer{conn: c, input: input, reader: bufio.NewReaderSize(input, 16*1024),
		body: make([]byte, len(f.response)), request: wire, fixture: f}, nil
}
func (p *peer) exchange() error {
	// Bounded whole response, reusable body storage, no reconnects or retries.
	p.input.N = int64(16*1024 + len(p.body))
	if err := p.conn.SetDeadline(time.Now().Add(5 * time.Second)); err != nil {
		return err
	}
	remaining := p.request
	for len(remaining) > 0 {
		n, err := p.conn.Write(remaining)
		if err != nil {
			return err
		}
		if n == 0 {
			return io.ErrShortWrite
		}
		remaining = remaining[n:]
	}
	response, err := http.ReadResponse(p.reader, nil)
	if err != nil {
		return err
	}
	defer response.Body.Close()
	if response.ProtoMajor != 1 || response.ProtoMinor != 1 || response.StatusCode != 200 || response.Close || response.ContentLength != int64(len(p.body)) || len(response.TransferEncoding) != 0 || len(response.Trailer) != 0 {
		return errors.New("response status/framing/reuse mismatch")
	}
	if _, err = io.ReadFull(response.Body, p.body); err != nil {
		return err
	}
	var extra [1]byte
	if n, err := response.Body.Read(extra[:]); n != 0 || err != io.EOF {
		return errors.New("response framing did not finish")
	}
	if !bytes.Equal(p.body, p.fixture.response) {
		return errors.New("response payload mismatch")
	}
	for _, h := range p.fixture.ResponseHeaders {
		if response.Header.Get(h[0]) != h[1] {
			return fmt.Errorf("response header %s mismatch", h[0])
		}
	}
	return nil
}
