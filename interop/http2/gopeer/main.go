// An independent raw HTTP/2 peer for cases python-h2 cannot represent.
package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"hash"
	"io"
	"net"
	"os"
	"strings"
	"syscall"
	"time"

	"golang.org/x/net/http2"
	"golang.org/x/net/http2/hpack"
)

const wireLimit = 16 << 20

type bounded struct {
	net.Conn
	read, written int
}

func (b *bounded) Read(p []byte) (int, error) {
	if b.read >= wireLimit {
		return 0, errors.New("read byte limit exceeded")
	}
	if len(p) > wireLimit-b.read {
		p = p[:wireLimit-b.read]
	}
	n, err := b.Conn.Read(p)
	b.read += n
	return n, err
}

func (b *bounded) Write(p []byte) (int, error) {
	if len(p) > wireLimit-b.written {
		return 0, errors.New("write byte limit exceeded")
	}
	n, err := b.Conn.Write(p)
	b.written += n
	return n, err
}

type peer struct {
	conn         *bounded
	framer       *http2.Framer
	encoder      *hpack.Encoder
	encoded      bytes.Buffer
	frames       int
	acks         int
	credit       int64
	window       int64
	updates      map[uint32]uint64
	resets       map[uint32]uint32
	streamSent   map[uint32]int64
	pushDisabled bool
	goaways      int
}

func newPeer(conn net.Conn) (*peer, error) {
	if err := conn.SetDeadline(time.Now().Add(10 * time.Second)); err != nil {
		return nil, err
	}
	p := &peer{
		conn: &bounded{Conn: conn}, credit: 65535, window: 65535,
		updates: make(map[uint32]uint64), resets: make(map[uint32]uint32),
		streamSent: make(map[uint32]int64),
	}
	p.framer = http2.NewFramer(p.conn, p.conn)
	p.framer.SetMaxReadFrameSize(16384)
	p.framer.ReadMetaHeaders = hpack.NewDecoder(4096, nil)
	p.framer.MaxHeaderListSize = 65536
	p.encoder = hpack.NewEncoder(&p.encoded)
	return p, nil
}

func (p *peer) fields(fields []hpack.HeaderField) ([]byte, error) {
	p.encoded.Reset()
	for _, field := range fields {
		if err := p.encoder.WriteField(field); err != nil {
			return nil, err
		}
	}
	return append([]byte(nil), p.encoded.Bytes()...), nil
}

func (p *peer) headers(stream uint32, fields []hpack.HeaderField, end, split bool) error {
	block, err := p.fields(fields)
	if err != nil {
		return err
	}
	if !split {
		return p.framer.WriteHeaders(http2.HeadersFrameParam{
			StreamID: stream, BlockFragment: block, EndHeaders: true, EndStream: end,
		})
	}
	if err := p.framer.WriteHeaders(http2.HeadersFrameParam{
		StreamID: stream, BlockFragment: block[:1], EndStream: end,
	}); err != nil {
		return err
	}
	return p.framer.WriteContinuation(stream, true, block[1:])
}

func (p *peer) read(ackSettings bool) (http2.Frame, error) {
	p.frames++
	if p.frames > 100000 {
		return nil, errors.New("frame count limit exceeded")
	}
	frame, err := p.framer.ReadFrame()
	if err != nil {
		return nil, err
	}
	switch frame := frame.(type) {
	case *http2.SettingsFrame:
		if frame.IsAck() {
			p.acks++
		} else {
			if err := frame.ForeachSetting(func(setting http2.Setting) error {
				switch setting.ID {
				case http2.SettingInitialWindowSize:
					p.window = int64(setting.Val)
				case http2.SettingHeaderTableSize:
					p.encoder.SetMaxDynamicTableSizeLimit(setting.Val)
					p.encoder.SetMaxDynamicTableSize(setting.Val)
				case http2.SettingEnablePush:
					p.pushDisabled = setting.Val == 0
				}
				return nil
			}); err != nil {
				return nil, err
			}
			if ackSettings {
				err = p.framer.WriteSettingsAck()
			}
		}
	case *http2.PingFrame:
		if !frame.IsAck() {
			err = p.framer.WritePing(true, frame.Data)
		}
	case *http2.WindowUpdateFrame:
		p.updates[frame.StreamID] += uint64(frame.Increment)
		if frame.StreamID == 0 {
			p.credit += int64(frame.Increment)
		}
	case *http2.RSTStreamFrame:
		p.resets[frame.StreamID] = uint32(frame.ErrCode)
	case *http2.GoAwayFrame:
		p.goaways++
	}
	return frame, err
}

func fields(status string, length int) []hpack.HeaderField {
	return []hpack.HeaderField{
		{Name: ":status", Value: status},
		{Name: "content-length", Value: fmt.Sprint(length)},
		{Name: "x-sync", Value: "promised-dynamic-table-value"},
	}
}

func (p *peer) body(stream uint32, data []byte, end bool) error {
	// Protocol scenarios use small bodies. Large valid workloads use CreditSender.
	available := p.window + int64(p.updates[stream]) - p.streamSent[stream]
	if int64(len(data)) > available || int64(len(data)) > p.credit {
		return errors.New("scenario exceeds the actual advertised send windows")
	}
	p.credit -= int64(len(data))
	p.streamSent[stream] += int64(len(data))
	for len(data) > 16384 {
		if err := p.framer.WriteData(stream, false, data[:16384]); err != nil {
			return err
		}
		data = data[16384:]
	}
	return p.framer.WriteData(stream, end, data)
}

func serve(conn net.Conn, scenario string) (map[string]any, error) {
	defer conn.Close()
	p, err := newPeer(conn)
	if err != nil {
		return nil, err
	}
	preface := make([]byte, len(http2.ClientPreface))
	if _, err := io.ReadFull(p.conn, preface); err != nil {
		return nil, err
	}
	if string(preface) != http2.ClientPreface {
		return nil, errors.New("invalid client preface")
	}
	if scenario == "admission-recovery" {
		return serveAdmission(p)
	}
	early := scenario == "early-response" || scenario == "early-response-app-cancel"
	resetDiscardPendingEnd := false
	resetDiscardRefundBarrier := false
	window := uint32(65535)
	if early {
		window = 1024
	}
	if err := p.framer.WriteSettings(http2.Setting{ID: http2.SettingInitialWindowSize, Val: window}); err != nil {
		return nil, err
	}
	requests := make(map[uint32][]hpack.HeaderField)
	requestEnded := make(map[uint32]bool)
	received := make(map[uint32]int)
	sent := make(map[uint32]bool)
	sawEOF, goaway := false, uint32(0)
	pendingSettings := 0
	resetUpdateBaseline := make(map[uint32]uint64)
	serverResets := make(map[uint32]uint32)
	earlyResponseBarrier := false
	for {
		frame, err := p.read(scenario != "push-before-ack")
		if errors.Is(err, io.EOF) || errors.Is(err, syscall.ECONNRESET) &&
			(scenario == "bad-continuation" || scenario == "push-after-ack") {
			sawEOF = true
			break
		}
		if err != nil {
			return nil, err
		}
		switch frame := frame.(type) {
		case *http2.SettingsFrame:
			if !frame.IsAck() {
				pendingSettings++
			}
		case *http2.MetaHeadersFrame:
			stream := frame.StreamID
			if len(requests) >= 8 || frame.Truncated {
				return nil, errors.New("request metadata limit exceeded")
			}
			requests[stream] = frame.Fields
			requestEnded[stream] = requestEnded[stream] || frame.StreamEnded()
			if scenario == "connect" {
				values := make(map[string]string)
				for _, field := range frame.Fields {
					values[field.Name] = field.Value
				}
				if values[":method"] != "CONNECT" || values[":authority"] == "" ||
					values[":scheme"] != "" || values[":path"] != "" {
					return nil, errors.New("invalid classic CONNECT pseudoheaders")
				}
				err = p.headers(stream, []hpack.HeaderField{{Name: ":status", Value: "200"}}, false, false)
			} else if early && stream == 1 {
				// Wait for upload DATA so the response races a blocked producer.
			} else if scenario == "reset-discard" {
				if stream == 1 {
					if err = p.headers(1, fields("200", 65535), false, false); err == nil {
						err = p.body(1, bytes.Repeat([]byte{1}, 1024), false)
					}
				}
				// Stream 3 waits for the reset and late DATA.
			} else if scenario == "reset-isolation" && stream == 1 {
				err = p.framer.WriteRSTStream(stream, http2.ErrCodeCancel)
			} else {
				if strings.HasPrefix(scenario, "push-") && stream == 1 {
					block, encodeErr := p.fields([]hpack.HeaderField{
						{Name: ":method", Value: "GET"}, {Name: ":scheme", Value: "http"},
						{Name: ":authority", Value: "localhost"}, {Name: ":path", Value: "/promised"},
						{Name: "x-sync", Value: "promised-dynamic-table-value"},
					})
					if encodeErr != nil {
						return nil, encodeErr
					}
					if err = p.framer.WritePushPromise(http2.PushPromiseParam{
						StreamID: stream, PromiseID: 2, BlockFragment: block, EndHeaders: true,
					}); err != nil {
						return nil, err
					}
					if scenario == "push-before-ack" {
						for i := 0; i < pendingSettings; i++ {
							if err := p.framer.WriteSettingsAck(); err != nil {
								return nil, err
							}
						}
					}
					if scenario == "push-after-ack" {
						continue
					}
				}
				if scenario == "goaway" {
					if err := p.framer.WriteGoAway(stream, http2.ErrCodeNo, nil); err != nil {
						return nil, err
					}
				}
				if scenario == "bad-continuation" {
					block, encodeErr := p.fields(fields("200", 37))
					if encodeErr != nil {
						return nil, encodeErr
					}
					if err := p.framer.WriteHeaders(http2.HeadersFrameParam{
						StreamID: stream, BlockFragment: block[:1],
					}); err != nil {
						return nil, err
					}
					if err := p.framer.WritePing(false, [8]byte{1}); err != nil {
						return nil, err
					}
					continue
				}
				length := 37
				status := "200"
				if scenario == "content-length" && stream == 1 {
					length = 38
				}
				if scenario == "no-body-data" && stream == 1 {
					status, length = "204", 0
				}
				responseFields := fields(status, length)
				if status == "204" {
					responseFields = []hpack.HeaderField{{Name: ":status", Value: "204"}}
				}
				if err = p.headers(stream, responseFields, false, scenario == "continuation"); err == nil {
					end := !(scenario == "content-length" && stream == 1)
					err = p.body(stream, bytes.Repeat([]byte{byte(stream % 251)}, 37), end)
					if err == nil && !end {
						err = p.framer.WritePing(false, [8]byte{2})
					}
				}
				sent[stream] = true
			}
		case *http2.DataFrame:
			received[frame.StreamID] += len(frame.Data())
			requestEnded[frame.StreamID] = requestEnded[frame.StreamID] || frame.StreamEnded()
			if scenario == "connect" {
				err = p.body(frame.StreamID, frame.Data(), frame.StreamEnded())
				sent[frame.StreamID] = frame.StreamEnded()
			} else if early && !sent[frame.StreamID] {
				if frame.StreamID != 1 || received[1] > 1024 {
					return nil, errors.New("upload exceeded withheld credit")
				}
				err = p.headers(frame.StreamID, fields("413", 0), true, false)
				sent[frame.StreamID] = true
				if err == nil && scenario == "early-response" {
					err = p.framer.WritePing(false, [8]byte{'e', 'a', 'r', 'l', 'y', 'e', 'n', 'd'})
				}
			}
		case *http2.GoAwayFrame:
			goaway = uint32(frame.ErrCode)
		case *http2.PingFrame:
			if scenario == "content-length" && frame.IsAck() && frame.Data == [8]byte{2} {
				err = p.body(1, nil, true)
			}
			if scenario == "early-response" && frame.IsAck() &&
				frame.Data == [8]byte{'e', 'a', 'r', 'l', 'y', 'e', 'n', 'd'} &&
				sent[1] && !earlyResponseBarrier {
				earlyResponseBarrier = true
				err = p.framer.WriteRSTStream(1, http2.ErrCodeNo)
				if err == nil {
					serverResets[1] = uint32(http2.ErrCodeNo)
				}
			}
		case *http2.RSTStreamFrame:
			if scenario == "reset-discard" && frame.StreamID == 1 {
				resetUpdateBaseline[1] = p.updates[1]
				// Deliberately late, discarded DATA stays within the credit that
				// existed before reset. This is not a normal body-flow test.
				if err = p.body(1, bytes.Repeat([]byte{1}, 32768), false); err != nil {
					return nil, err
				}
				if err = p.headers(3, fields("200", 37), false, false); err == nil {
					err = p.body(3, bytes.Repeat([]byte{3}, 37), false)
				}
				resetDiscardPendingEnd = true
			}
		}
		if err != nil {
			return nil, err
		}
		// Keep a response open until discarded DATA credit actually returns.
		// Otherwise graceful close can legitimately discard pending refunds.
		if resetDiscardPendingEnd && p.updates[0] >= 1024+32768 {
			if err := p.body(3, nil, true); err != nil {
				return nil, err
			}
			resetDiscardPendingEnd = false
			resetDiscardRefundBarrier = true
			sent[3] = true
		}
	}
	return map[string]any{
		"requests": len(requests), "received": received, "sent": sent,
		"resets": p.resets, "goaway": goaway, "peer_eof": sawEOF,
		"window_updates": p.updates, "bytes_in": p.conn.read, "bytes_out": p.conn.written,
		"push_disabled": p.pushDisabled, "goaway_count": p.goaways,
		"stream_updates_at_reset": resetUpdateBaseline,
		"server_resets":           serverResets, "early_response_barrier": earlyResponseBarrier,
		"request_ended":                requestEnded,
		"reset_discard_refund_barrier": resetDiscardRefundBarrier,
	}, nil
}

type request struct {
	Method    string     `json:"method"`
	Path      string     `json:"path"`
	BodyBytes int        `json:"body_bytes"`
	Trailers  [][]string `json:"trailers"`
}

type spec struct {
	Host         string    `json:"host"`
	Port         int       `json:"port"`
	Requests     []request `json:"requests"`
	RequestCount int       `json:"request_count"`
	WireScenario string    `json:"wire_scenario"`
	Actions      []struct {
		Action     string `json:"action"`
		StreamID   uint32 `json:"stream_id"`
		AfterBytes int    `json:"after_bytes"`
		Code       uint32 `json:"code"`
	} `json:"actions"`
}

type result struct {
	StreamID      uint32     `json:"stream_id"`
	Status        *int       `json:"status"`
	ContentLength *int       `json:"content_length"`
	Bytes         int        `json:"bytes"`
	SHA256        string     `json:"sha256"`
	Trailers      [][]string `json:"trailers"`
	Informational []int      `json:"informational"`
	Ended         bool       `json:"ended"`
	Outcome       string     `json:"outcome"`
	Error         any        `json:"error"`
	digest        hash.Hash
	length        int
}

// rawClient supplies independent Framer/HPACK coverage for GOAWAY and CONNECT.
// It is not a production client and is never used for large flow workloads.
func rawClient(input spec) (map[string]any, error) {
	for _, action := range input.Actions {
		switch action.Action {
		case "reset", "graceful_close":
		case "cancel_upload_after_response":
			if action.StreamID == 0 || action.StreamID%2 == 0 ||
				uint64(action.StreamID/2) >= uint64(len(input.Requests)) {
				return nil, errors.New("invalid cancellation stream")
			}
		default:
			return nil, fmt.Errorf("unsupported Go protocol action %q", action.Action)
		}
	}
	for _, req := range input.Requests {
		if req.BodyBytes < 0 || req.BodyBytes > 131087 {
			return nil, errors.New("Go protocol upload exceeds its 131087-byte probe bound")
		}
	}
	conn, err := net.DialTimeout("tcp", net.JoinHostPort(input.Host, fmt.Sprint(input.Port)), 5*time.Second)
	if err != nil {
		return nil, err
	}
	defer conn.Close()
	p, err := newPeer(conn)
	if err != nil {
		return nil, err
	}
	if _, err := io.WriteString(p.conn, http2.ClientPreface); err != nil {
		return nil, err
	}
	if err := p.framer.WriteSettings(http2.Setting{ID: http2.SettingEnablePush, Val: 0}); err != nil {
		return nil, err
	}
	results := make(map[uint32]*result)
	requestEnded := make(map[uint32]bool)
	for index, req := range input.Requests {
		stream := uint32(index*2 + 1)
		headers := []hpack.HeaderField{{Name: ":method", Value: req.Method}, {Name: ":authority", Value: "localhost"}}
		if req.Method != "CONNECT" {
			headers = append(headers, hpack.HeaderField{Name: ":scheme", Value: "http"}, hpack.HeaderField{Name: ":path", Value: req.Path})
		}
		requestEnded[stream] = req.BodyBytes == 0 && len(req.Trailers) == 0
		if err := p.headers(stream, headers, requestEnded[stream], input.WireScenario == "request-continuation"); err != nil {
			return nil, err
		}
		results[stream] = &result{
			StreamID: stream, Trailers: [][]string{}, Informational: []int{},
			digest: sha256.New(), length: -1,
		}
	}
	done := 0
	receiveDone := make(map[uint32]bool)
	markDone := func(stream uint32) {
		if !receiveDone[stream] {
			receiveDone[stream] = true
			done++
		}
	}
	var connectionError any
	// Uploads start only after server SETTINGS. CONNECT waits for status 200.
	uploaded := make(map[uint32]bool)
	uploadProgress := make(map[uint32]int)
	pendingUpload := func() bool {
		for index, req := range input.Requests {
			stream := uint32(index*2 + 1)
			if req.BodyBytes > 0 && !uploaded[stream] && results[stream].Error == nil {
				return true
			}
		}
		return false
	}
	for done < len(results) || pendingUpload() {
		frame, err := p.read(true)
		if err != nil {
			// ReadMetaHeaders enforces uninterrupted CONTINUATION sequences.
			var connectionErr http2.ConnectionError
			if errors.As(err, &connectionErr) {
				connectionError = map[string]any{"scope": "connection", "code": uint32(connectionErr)}
				_ = p.framer.WriteGoAway(0, http2.ErrCode(connectionErr), nil)
				break
			}
			return nil, err
		}
		switch frame := frame.(type) {
		case *http2.MetaHeadersFrame:
			r := results[frame.StreamID]
			if r == nil {
				return nil, errors.New("unexpected response stream")
			}
			for _, field := range frame.Fields {
				if field.Name == ":status" {
					var status int
					if _, err := fmt.Sscan(field.Value, &status); err != nil {
						return nil, err
					}
					if status < 200 {
						r.Informational = append(r.Informational, status)
					} else {
						r.Status = &status
					}
				} else if field.Name == "content-length" {
					if _, err := fmt.Sscan(field.Value, &r.length); err != nil {
						return nil, err
					}
					length := r.length
					r.ContentLength = &length
				}
			}
			if frame.StreamEnded() {
				r.Ended = true
				markDone(frame.StreamID)
			}
		case *http2.DataFrame:
			r := results[frame.StreamID]
			if r == nil {
				return nil, errors.New("unexpected DATA stream")
			}
			if r.Error != nil {
				if frame.Length > 0 {
					if err := p.framer.WriteWindowUpdate(0, frame.Length); err != nil {
						return nil, err
					}
				}
				continue
			}
			if r.Status != nil && (*r.Status == 204 || *r.Status == 304) && len(frame.Data()) > 0 {
				r.Error = map[string]any{"scope": "stream", "code": 1}
				if err := p.framer.WriteRSTStream(frame.StreamID, http2.ErrCodeProtocol); err != nil {
					return nil, err
				}
				if err := p.framer.WriteWindowUpdate(0, frame.Length); err != nil {
					return nil, err
				}
				markDone(frame.StreamID)
				continue
			}
			r.Bytes += len(frame.Data())
			_, _ = r.digest.Write(frame.Data())
			reset := false
			for _, action := range input.Actions {
				if action.Action == "reset" && action.StreamID == frame.StreamID && r.Bytes >= action.AfterBytes {
					if err := p.framer.WriteRSTStream(frame.StreamID, http2.ErrCode(action.Code)); err != nil {
						return nil, err
					}
					r.Error = map[string]any{"scope": "stream", "code": action.Code}
					markDone(frame.StreamID)
					reset = true
				}
			}
			if frame.Length > 0 {
				if err := p.framer.WriteWindowUpdate(0, frame.Length); err != nil {
					return nil, err
				}
			}
			if reset {
				continue
			}
			if frame.Length > 0 && !frame.StreamEnded() {
				if err := p.framer.WriteWindowUpdate(frame.StreamID, frame.Length); err != nil {
					return nil, err
				}
			}
			if frame.StreamEnded() {
				if r.length >= 0 && r.length != r.Bytes {
					r.Error = map[string]any{"scope": "stream", "code": 1}
					_ = p.framer.WriteRSTStream(frame.StreamID, http2.ErrCodeProtocol)
				} else {
					r.Ended = true
				}
				markDone(frame.StreamID)
			}
		case *http2.RSTStreamFrame:
			r := results[frame.StreamID]
			r.Error = map[string]any{"scope": "stream", "code": uint32(frame.ErrCode)}
			markDone(frame.StreamID)
		case *http2.PushPromiseFrame:
			// Push header decoding must share history with ReadMetaHeaders.
			if !frame.HeadersEnded() {
				return nil, errors.New("reference requires complete PUSH_PROMISE")
			}
			if _, err := p.framer.ReadMetaHeaders.DecodeFull(frame.HeaderBlockFragment()); err != nil {
				return nil, err
			}
			if p.acks > 0 {
				connectionError = map[string]any{"scope": "connection", "code": 1}
				_ = p.framer.WriteGoAway(0, http2.ErrCodeProtocol, nil)
				done = len(results)
			} else {
				if err := p.framer.WriteRSTStream(frame.PromiseID, http2.ErrCodeCancel); err != nil {
					return nil, err
				}
			}
		case *http2.GoAwayFrame:
			if frame.ErrCode != http2.ErrCodeNo {
				connectionError = map[string]any{"scope": "connection", "code": uint32(frame.ErrCode)}
				done = len(results)
			}
		}
		if connectionError != nil {
			break
		}
		for _, action := range input.Actions {
			r := results[action.StreamID]
			if action.Action == "cancel_upload_after_response" && r != nil &&
				r.Ended && r.Error == nil && !requestEnded[action.StreamID] {
				if err := p.framer.WriteRSTStream(action.StreamID, http2.ErrCodeCancel); err != nil {
					return nil, err
				}
				r.Error = map[string]any{"scope": "stream", "code": uint32(http2.ErrCodeCancel)}
				uploaded[action.StreamID] = true
			}
		}
		for {
			progress := false
			for index, req := range input.Requests {
				stream := uint32(index*2 + 1)
				r := results[stream]
				if req.BodyBytes > 0 && !uploaded[stream] && r.Error == nil && (req.Method != "CONNECT" || r.Status != nil) {
					if req.Method == "CONNECT" && uploadProgress[stream] > 0 && r.Bytes == 0 {
						continue
					}
					// Only small protocol probes use this producer.
					available := p.window + int64(p.updates[stream]) - p.streamSent[stream]
					amount := min(req.BodyBytes-uploadProgress[stream], int(available), int(p.credit), 16384)
					if req.Method == "CONNECT" && uploadProgress[stream] == 0 && req.BodyBytes > 1 {
						amount = min(amount, req.BodyBytes-1, 17)
					}
					if amount <= 0 {
						continue
					}
					final := uploadProgress[stream]+amount == req.BodyBytes
					if err := p.body(stream, bytes.Repeat([]byte{byte(stream % 251)}, amount), final && len(req.Trailers) == 0); err != nil {
						return nil, err
					}
					requestEnded[stream] = final && len(req.Trailers) == 0
					if len(req.Trailers) > 0 {
						if !final {
							return nil, errors.New("reference trailers require a one-frame upload")
						}
						fields := make([]hpack.HeaderField, 0, len(req.Trailers))
						for _, trailer := range req.Trailers {
							if len(trailer) != 2 {
								return nil, errors.New("invalid trailer pair")
							}
							fields = append(fields, hpack.HeaderField{Name: trailer[0], Value: trailer[1]})
						}
						if err := p.headers(stream, fields, true, false); err != nil {
							return nil, err
						}
						requestEnded[stream] = true
					}
					uploadProgress[stream] += amount
					uploaded[stream] = final
					progress = true
				}
			}
			if !progress {
				break
			}
		}
	}
	ordered := make([]*result, 0, len(results))
	for index := range input.Requests {
		r := results[uint32(index*2+1)]
		r.SHA256 = hex.EncodeToString(r.digest.Sum(nil))
		switch {
		case r.Error != nil:
			r.Outcome = "reset"
		case r.Ended && requestEnded[r.StreamID]:
			r.Outcome = "complete"
		default:
			r.Outcome = "connection_failed"
		}
		ordered = append(ordered, r)
	}
	for _, action := range input.Actions {
		if action.Action == "graceful_close" {
			if err := p.framer.WriteGoAway(0, http2.ErrCodeNo, nil); err != nil {
				return nil, err
			}
		}
	}
	if err := conn.Close(); err != nil {
		return nil, err
	}
	connectionOutcome := "graceful"
	if connectionError != nil {
		connectionOutcome = "protocol"
	}
	return map[string]any{
		"schema": 1, "streams": ordered,
		"connection": map[string]any{"closed": true, "error": connectionError, "outcome": connectionOutcome},
	}, nil
}

func run() error {
	scenario := flag.String("scenario", "", "fixed protocol scenario")
	report := flag.String("report", "", "bounded server report file")
	clientFile := flag.String("client", "", "client request file")
	resultFile := flag.String("result", "", "client result file")
	flag.Parse()
	if *clientFile != "" {
		file, err := os.Open(*clientFile)
		if err != nil {
			return err
		}
		raw, err := io.ReadAll(io.LimitReader(file, 65537))
		_ = file.Close()
		if err != nil {
			return err
		}
		if len(raw) > 65536 {
			return errors.New("request JSON exceeds limit")
		}
		var input spec
		if err := json.Unmarshal(raw, &input); err != nil {
			return err
		}
		if len(input.Requests) < 1 || len(input.Requests) > 8 || input.RequestCount != len(input.Requests) {
			return errors.New("invalid reference request count")
		}
		output, err := rawClient(input)
		if err != nil {
			return err
		}
		raw, err = json.Marshal(output)
		if err != nil {
			return err
		}
		return os.WriteFile(*resultFile, raw, 0600)
	}
	allowed := map[string]bool{
		"connect": true, "goaway": true, "continuation": true,
		"bad-continuation": true, "push-before-ack": true, "push-after-ack": true,
		"reset-isolation": true, "content-length": true, "early-response": true,
		"reset-discard": true, "graceful-close": true,
		"no-body-data":              true,
		"early-response-app-cancel": true,
		"admission-recovery":        true,
	}
	if !allowed[*scenario] || *report == "" {
		return errors.New("server requires a known scenario and report file")
	}
	listener, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		return err
	}
	defer listener.Close()
	if err := listener.(*net.TCPListener).SetDeadline(time.Now().Add(10 * time.Second)); err != nil {
		return err
	}
	fmt.Printf("LISTEN %s\n", listener.Addr())
	conn, err := listener.Accept()
	if err != nil {
		return err
	}
	output, err := serve(conn, *scenario)
	if err != nil {
		return err
	}
	raw, err := json.Marshal(output)
	if err != nil {
		return err
	}
	return os.WriteFile(*report, raw, 0600)
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
