// Independent positive WebSocket peer using only the Go standard library.
package main

import (
	"bufio"
	"bytes"
	"crypto/rand"
	"crypto/sha1"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"strings"
	"time"
)

const maxPayload = 1 << 20

type frame struct {
	opcode byte
	fin    bool
	body   []byte
}

func writeFrame(w io.Writer, opcode byte, payload []byte) error {
	if len(payload) > maxPayload {
		return errors.New("payload exceeds peer limit")
	}
	wire := []byte{0x80 | opcode}
	switch {
	case len(payload) < 126:
		wire = append(wire, 0x80|byte(len(payload)))
	case len(payload) <= 65535:
		wire = binary.BigEndian.AppendUint16(append(wire, 0xfe), uint16(len(payload)))
	default:
		wire = binary.BigEndian.AppendUint64(append(wire, 0xff), uint64(len(payload)))
	}
	var mask [4]byte
	if _, err := rand.Read(mask[:]); err != nil {
		return err
	}
	wire = append(wire, mask[:]...)
	for i, value := range payload {
		wire = append(wire, value^mask[i%4])
	}
	for len(wire) != 0 {
		n, err := w.Write(wire)
		if err != nil {
			return err
		}
		if n == 0 {
			return io.ErrNoProgress
		}
		wire = wire[n:]
	}
	return nil
}

func readFrame(r io.Reader) (frame, error) {
	var prefix [2]byte
	if _, err := io.ReadFull(r, prefix[:]); err != nil {
		return frame{}, err
	}
	result := frame{opcode: prefix[0] & 15, fin: prefix[0]&0x80 != 0}
	if prefix[0]&0x70 != 0 || prefix[1]&0x80 != 0 {
		return result, errors.New("server frame is masked or has RSV bits")
	}
	switch result.opcode {
	case 0, 1, 2, 8, 9, 10:
	default:
		return result, errors.New("reserved opcode")
	}
	size := uint64(prefix[1] & 127)
	if size == 126 {
		var extended [2]byte
		if _, err := io.ReadFull(r, extended[:]); err != nil {
			return result, err
		}
		size = uint64(binary.BigEndian.Uint16(extended[:]))
		if size < 126 {
			return result, errors.New("nonminimal 16-bit length")
		}
	} else if size == 127 {
		var extended [8]byte
		if _, err := io.ReadFull(r, extended[:]); err != nil {
			return result, err
		}
		size = binary.BigEndian.Uint64(extended[:])
		if size <= 65535 || size>>63 != 0 {
			return result, errors.New("invalid 64-bit length")
		}
	}
	if size > maxPayload || (result.opcode >= 8 && (size > 125 || !result.fin)) {
		return result, errors.New("oversized frame or invalid control")
	}
	result.body = make([]byte, int(size))
	_, err := io.ReadFull(r, result.body)
	return result, err
}

func expectMessage(r io.Reader, w io.Writer, opcode byte, expected []byte) error {
	var received []byte
	started := false
	for i := 0; i < 128; i++ {
		f, err := readFrame(r)
		if err != nil {
			return err
		}
		if f.opcode == 9 {
			if err := writeFrame(w, 10, f.body); err != nil {
				return err
			}
			continue
		}
		if (!started && f.opcode != opcode) || (started && f.opcode != 0) {
			return errors.New("wrong message opcode or continuation")
		}
		started = true
		if len(received)+len(f.body) > maxPayload {
			return errors.New("message exceeds peer limit")
		}
		received = append(received, f.body...)
		if f.fin {
			if !bytes.Equal(received, expected) {
				return errors.New("message payload mismatch")
			}
			return nil
		}
	}
	return errors.New("too many frames")
}

func run(address string) error {
	host, _, err := net.SplitHostPort(address)
	if err != nil || !net.ParseIP(host).IsLoopback() {
		return errors.New("expected loopback host:port")
	}
	conn, err := net.DialTimeout("tcp", address, 5*time.Second)
	if err != nil {
		return err
	}
	defer conn.Close()
	if err := conn.SetDeadline(time.Now().Add(5 * time.Second)); err != nil {
		return err
	}
	var nonce [16]byte
	if _, err := rand.Read(nonce[:]); err != nil {
		return err
	}
	key := base64.StdEncoding.EncodeToString(nonce[:])
	req, err := http.NewRequest("GET", "http://"+address+"/chat", nil)
	if err != nil {
		return err
	}
	req.Header.Set("Connection", "Upgrade")
	req.Header.Set("Upgrade", "websocket")
	req.Header.Set("Sec-WebSocket-Version", "13")
	req.Header.Set("Sec-WebSocket-Key", key)
	if err := req.Write(conn); err != nil {
		return err
	}
	bounded := &io.LimitedReader{R: conn, N: 65536}
	reader := bufio.NewReader(bounded)
	response, err := http.ReadResponse(reader, req)
	if err != nil {
		return err
	}
	bounded.N = 4 * maxPayload
	digest := sha1.Sum([]byte(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"))
	connectionUpgrade := false
	for _, value := range response.Header.Values("Connection") {
		for _, token := range strings.Split(value, ",") {
			connectionUpgrade = connectionUpgrade || strings.EqualFold(strings.TrimSpace(token), "upgrade")
		}
	}
	if response.StatusCode != 101 || response.Proto != "HTTP/1.1" || !connectionUpgrade ||
		len(response.Header.Values("Upgrade")) != 1 ||
		!strings.EqualFold(response.Header.Get("Upgrade"), "websocket") ||
		len(response.Header.Values("Content-Length")) != 0 ||
		len(response.Header.Values("Transfer-Encoding")) != 0 ||
		len(response.Header.Values("Sec-WebSocket-Accept")) != 1 ||
		response.Header.Get("Sec-WebSocket-Accept") != base64.StdEncoding.EncodeToString(digest[:]) ||
		len(response.Header.Values("Sec-WebSocket-Extensions")) != 0 ||
		len(response.Header.Values("Sec-WebSocket-Protocol")) != 0 {
		return errors.New("invalid upgrade response")
	}
	text := []byte("independent-Go-\u20ac")
	body := make([]byte, 65792)
	for i := range body {
		body[i] = byte(i)
	}
	for _, item := range []frame{{opcode: 1, body: text}, {opcode: 2, body: body}} {
		if err := writeFrame(conn, item.opcode, item.body); err != nil {
			return err
		}
		if err := expectMessage(reader, conn, item.opcode, item.body); err != nil {
			return err
		}
	}
	if err := writeFrame(conn, 9, []byte("Go-ping")); err != nil {
		return err
	}
	pong, err := readFrame(reader)
	if err != nil {
		return err
	}
	if pong.opcode != 10 || string(pong.body) != "Go-ping" {
		return errors.New("pong mismatch")
	}
	closeBody := append([]byte{0x03, 0xe8}, []byte("done")...)
	if err := writeFrame(conn, 8, closeBody); err != nil {
		return err
	}
	closing, err := readFrame(reader)
	if err != nil {
		return err
	}
	if closing.opcode != 8 || !bytes.Equal(closing.body, closeBody) {
		return errors.New("close response mismatch")
	}
	if _, err := reader.ReadByte(); err != io.EOF {
		return fmt.Errorf("expected EOF after close, got %v", err)
	}
	bodyDigest := sha256.Sum256(body)
	return json.NewEncoder(os.Stdout).Encode(map[string]any{
		"text": string(text), "binary_bytes": len(body), "binary_sha256": fmt.Sprintf("%x", bodyDigest),
		"pong": true, "close": 1000, "clean_eof": true,
	})
}

func main() {
	address := flag.String("address", "", "loopback server host:port")
	flag.Parse()
	if err := run(*address); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
