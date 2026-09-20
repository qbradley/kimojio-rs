package main

import (
	"errors"
	"fmt"
	"io"

	"golang.org/x/net/http2"
)

var admissionZeroPing = [8]byte{'a', 'd', 'm', '-', 'z', 'e', 'r', 'o'}
var admissionRetiredPing = [8]byte{'a', 'd', 'm', '-', 'd', 'o', 'n', 'e'}

func serveAdmission(p *peer) (map[string]any, error) {
	if err := p.framer.WriteSettings(http2.Setting{ID: http2.SettingMaxConcurrentStreams, Val: 1}); err != nil {
		return nil, err
	}
	stage := 0
	requests := 0
	positiveAcknowledged := false
	events := make([]string, 0, 9)
	for {
		frame, err := p.read(true)
		if errors.Is(err, io.EOF) {
			if stage != 4 || requests != 2 {
				return nil, errors.New("connection closed before queued admission recovered")
			}
			return map[string]any{
				"requests": requests, "peer_eof": true, "resets": p.resets,
				"goaway": uint32(0), "goaway_count": p.goaways,
				"admission_events": events, "request_ended": map[uint32]bool{1: true, 3: true},
				"bytes_in": p.conn.read, "bytes_out": p.conn.written,
			}, nil
		}
		if err != nil {
			return nil, err
		}
		switch frame := frame.(type) {
		case *http2.SettingsFrame:
			if stage == 3 && frame.IsAck() && p.acks == 3 {
				positiveAcknowledged = true
				events = append(events, "positive-settings-ack")
			}
		case *http2.MetaHeadersFrame:
			if stage != 0 && stage != 3 {
				return nil, errors.New("request HEADERS before positive SETTINGS")
			}
			expected := uint32(1)
			if stage == 3 {
				if !positiveAcknowledged {
					return nil, errors.New("request HEADERS before positive SETTINGS acknowledgment")
				}
				expected = 3
			}
			if frame.StreamID != expected || !frame.StreamEnded() || frame.Truncated {
				return nil, errors.New("unexpected admission request stream or body")
			}
			requests++
			if stage == 0 {
				events = append(events, "request-1", "settings-zero")
				if err := p.framer.WriteSettings(http2.Setting{ID: http2.SettingMaxConcurrentStreams, Val: 0}); err != nil {
					return nil, err
				}
				if err := p.framer.WritePing(false, admissionZeroPing); err != nil {
					return nil, err
				}
				stage = 1
			} else {
				events = append(events, "request-3", "response-3-end")
				if err := p.headers(3, fields("200", 0), true, false); err != nil {
					return nil, err
				}
				stage = 4
			}
		case *http2.PingFrame:
			if !frame.IsAck() {
				continue
			}
			switch frame.Data {
			case admissionZeroPing:
				if stage != 1 || p.acks < 2 {
					return nil, errors.New("zero barrier preceded SETTINGS acknowledgments")
				}
				events = append(events, "zero-ping-ack", "response-1-end")
				if err := p.headers(1, fields("200", 0), true, false); err != nil {
					return nil, err
				}
				if err := p.framer.WritePing(false, admissionRetiredPing); err != nil {
					return nil, err
				}
				stage = 2
			case admissionRetiredPing:
				if stage != 2 || requests != 1 {
					return nil, errors.New("invalid retirement barrier while admission is zero")
				}
				events = append(events, "retirement-ping-ack", "settings-one")
				if err := p.framer.WriteSettings(http2.Setting{ID: http2.SettingMaxConcurrentStreams, Val: 1}); err != nil {
					return nil, err
				}
				stage = 3
			}
		case *http2.DataFrame:
			return nil, errors.New("unexpected admission request DATA")
		case *http2.RSTStreamFrame:
			return nil, fmt.Errorf("admission request reset: %v", frame.ErrCode)
		case *http2.GoAwayFrame:
			if stage != 4 || frame.ErrCode != http2.ErrCodeNo {
				return nil, errors.New("GOAWAY before successful admission recovery")
			}
		}
	}
}
