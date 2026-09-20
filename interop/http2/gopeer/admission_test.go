package main

import (
	"io"
	"net"
	"strings"
	"testing"
	"time"

	"golang.org/x/net/http2"
	"golang.org/x/net/http2/hpack"
)

func TestAdmissionRecoveryAndFaultControls(t *testing.T) {
	for _, premature := range []string{"", "zero", "retired", "after-barrier", "drop-resume"} {
		t.Run("premature-"+premature, func(t *testing.T) {
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
					report, err = serve(conn, "admission-recovery")
				}
				done <- outcome{report, err}
			}()
			clientErr := func() error {
				conn, err := net.DialTimeout("tcp4", listener.Addr().String(), time.Second)
				if err != nil {
					return err
				}
				defer conn.Close()
				p, err := newPeer(conn)
				if err != nil {
					return err
				}
				if err := conn.SetDeadline(time.Now().Add(3 * time.Second)); err != nil {
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
					{Name: ":authority", Value: "localhost"}, {Name: ":path", Value: "/"},
				}
				if err := p.headers(1, headers, true, false); err != nil {
					return err
				}
				zeroSeen, responses := false, 0
				for frames := 0; frames < 32; frames++ {
					frame, err := p.framer.ReadFrame()
					if err != nil {
						return err
					}
					switch frame := frame.(type) {
					case *http2.SettingsFrame:
						if frame.IsAck() {
							continue
						}
						reopened := false
						if err := frame.ForeachSetting(func(setting http2.Setting) error {
							if setting.ID == http2.SettingMaxConcurrentStreams {
								reopened = zeroSeen && setting.Val == 1
								zeroSeen = zeroSeen || setting.Val == 0
							}
							return nil
						}); err != nil {
							return err
						}
						if err := p.framer.WriteSettingsAck(); err != nil {
							return err
						}
						if reopened && premature == "" {
							if err := p.headers(3, headers, true, false); err != nil {
								return err
							}
						}
						if reopened && premature == "drop-resume" {
							if err := conn.SetReadDeadline(time.Now().Add(100 * time.Millisecond)); err != nil {
								return err
							}
						}
					case *http2.PingFrame:
						if premature == "zero" && frame.Data == admissionZeroPing ||
							premature == "retired" && frame.Data == admissionRetiredPing {
							if err := p.headers(3, headers, true, false); err != nil {
								return err
							}
						}
						if err := p.framer.WritePing(true, frame.Data); err != nil {
							return err
						}
						if premature == "after-barrier" && frame.Data == admissionRetiredPing {
							if err := p.headers(3, headers, true, false); err != nil {
								return err
							}
						}
					case *http2.MetaHeadersFrame:
						if frame.StreamEnded() {
							responses++
						}
						if responses == 2 {
							return p.framer.WriteGoAway(0, http2.ErrCodeNo, nil)
						}
					}
				}
				return io.ErrNoProgress
			}()
			server := <-done
			if premature == "drop-resume" {
				if server.err == nil || !strings.Contains(server.err.Error(), "before queued admission recovered") {
					t.Fatalf("dropped request escaped oracle: client=%v server=%v", clientErr, server.err)
				}
			} else if premature != "" {
				if server.err == nil || !strings.Contains(server.err.Error(), "HEADERS before positive SETTINGS") {
					t.Fatalf("premature headers escaped oracle: client=%v server=%v", clientErr, server.err)
				}
			} else if clientErr != nil || server.err != nil || server.report["requests"] != 2 {
				t.Fatalf("recovery failed: client=%v server=%v report=%v", clientErr, server.err, server.report)
			}
		})
	}
}
