package main

import (
	"bytes"
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
	"unicode/utf8"

	"github.com/gorilla/websocket"
)

type serverFlags struct {
	bind, root, mode                                                         string
	clients, message, queue, clientBytes, totalBytes, sendBuffer, frameBytes int
	lifetime, writeTimeout, closeTimeout                                     time.Duration
	noDelay                                                                  bool
	keepAlive                                                                bool
	maxRequests                                                              int
}

func parseServer(args []string) (serverFlags, error) {
	var c serverFlags
	f := flag.NewFlagSet("serve", flag.ContinueOnError)
	f.StringVar(&c.bind, "bind", "127.0.0.1:0", "loopback listener")
	f.StringVar(&c.root, "root", "", "static file root")
	f.StringVar(&c.mode, "mode", "fixture", "fixture or static")
	f.IntVar(&c.clients, "max-clients", 64, "admitted connections")
	f.IntVar(&c.message, "max-message-bytes", 1<<20, "message cap")
	f.IntVar(&c.queue, "max-queued-messages", 32, "queued AND in-flight messages")
	f.IntVar(&c.clientBytes, "max-client-bytes", 4<<20, "queued/inflight payload cap")
	f.IntVar(&c.totalBytes, "max-total-bytes", 32<<20, "unique retained payload cap, excludes metadata")
	f.IntVar(&c.sendBuffer, "send-buffer-bytes", 0, "requested SO_SNDBUF; zero retains OS default")
	f.IntVar(&c.frameBytes, "frame-bytes", 16384, "gorilla write buffer/frame size")
	f.DurationVar(&c.lifetime, "lifetime", 60*time.Second, "bounded server lifetime")
	f.DurationVar(&c.writeTimeout, "write-timeout", 5*time.Second, "write deadline")
	f.DurationVar(&c.closeTimeout, "close-timeout", 5*time.Second, "control close deadline")
	f.BoolVar(&c.noDelay, "nodelay", false, "accepted socket TCP_NODELAY")
	f.BoolVar(&c.keepAlive, "keepalive", false, "enable TCP keepalive30s/1s/30probes")
	f.IntVar(&c.maxRequests, "max-requests-per-connection", 1000, "HTTP exchanges per connection; zero is uncapped")
	if err := f.Parse(args); err != nil {
		return c, err
	}
	host, _, err := net.SplitHostPort(c.bind)
	if err != nil || host != "127.0.0.1" || c.clients < 1 || c.clients > 256 ||
		c.message < 8 || c.message > 1<<20 || c.queue < 1 || c.queue > 1024 ||
		c.clientBytes < c.message || c.totalBytes < c.message || c.sendBuffer < 0 ||
		c.frameBytes < 128 || c.frameBytes > 1<<20 || c.maxRequests < 0 || c.maxRequests > 1000000000 ||
		c.lifetime <= 0 || c.lifetime > 5*time.Minute ||
		c.writeTimeout <= 0 || c.writeTimeout > 15*time.Second || c.closeTimeout <= 0 || c.closeTimeout > 15*time.Second {
		return c, errors.New("invalid bounded server configuration")
	}
	return c, nil
}

type admittedConn struct {
	*net.TCPConn
	once    sync.Once
	release func()
}

func (c *admittedConn) Close() error {
	err := c.TCPConn.Close()
	c.once.Do(c.release)
	return err
}

type boundedListener struct {
	net.Listener
	slots  chan struct{}
	config serverFlags
}

func (l *boundedListener) Accept() (net.Conn, error) {
	for {
		conn, err := l.Listener.Accept()
		if err != nil {
			return nil, err
		}
		select {
		case l.slots <- struct{}{}:
			tcp := conn.(*net.TCPConn)
			if err := tcp.SetNoDelay(l.config.noDelay); err != nil {
				tcp.Close()
				<-l.slots
				return nil, err
			}
			if err := configureKeepAlive(tcp, l.config.keepAlive); err != nil {
				tcp.Close()
				<-l.slots
				return nil, err
			}
			if l.config.sendBuffer > 0 {
				if err := tcp.SetWriteBuffer(l.config.sendBuffer); err != nil {
					tcp.Close()
					<-l.slots
					return nil, err
				}

			}
			return &admittedConn{TCPConn: tcp, release: func() { <-l.slots }}, nil
		default:
			conn.Close()
		}
	}
}
func configureKeepAlive(conn *net.TCPConn, enabled bool) error {
	if err := conn.SetKeepAlive(enabled); err != nil || !enabled {
		return err
	}
	raw, err := conn.SyscallConn()
	if err != nil {
		return err
	}
	var optionError error
	err = raw.Control(func(fd uintptr) {
		for _, option := range []struct{ name, value int }{
			{syscall.TCP_KEEPIDLE, 30}, {syscall.TCP_KEEPINTVL, 1}, {syscall.TCP_KEEPCNT, 30},
		} {
			if optionError == nil {
				optionError = syscall.SetsockoptInt(int(fd), syscall.IPPROTO_TCP, option.name, option.value)
			}
		}
	})
	if err != nil {
		return err
	}
	return optionError
}

func listen(c serverFlags) (*boundedListener, error) {
	raw, err := net.Listen("tcp4", c.bind)
	if err != nil {
		return nil, err
	}
	return &boundedListener{Listener: raw, slots: make(chan struct{}, c.clients), config: c}, nil
}
func runServer(c serverFlags, handler http.Handler, shutdown func()) error {
	listener, err := listen(c)
	if err != nil {
		return err
	}
	server := &http.Server{Handler: handler, ReadHeaderTimeout: 5 * time.Second, IdleTimeout: 60 * time.Second,
		ReadTimeout: 30 * time.Second, WriteTimeout: 30 * time.Second, MaxHeaderBytes: 65536}
	applyRequestCap(server, c.maxRequests)
	finished := make(chan error, 1)
	go func() { finished <- server.Serve(listener) }()
	fmt.Printf("LISTEN %s\n", listener.Addr())
	signals := make(chan os.Signal, 1)
	signal.Notify(signals, syscall.SIGTERM, syscall.SIGINT)
	defer signal.Stop(signals)
	timer := time.NewTimer(c.lifetime)
	defer timer.Stop()
	select {
	case <-timer.C:
	case <-signals:
	case err := <-finished:
		return err
	}

	if shutdown != nil {
		shutdown()
	}
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	server.Shutdown(ctx)
	server.Close()
	return nil
}

type requestCounterKey struct{}

func applyRequestCap(server *http.Server, limit int) {
	if limit == 0 {
		return
	}
	handler := server.Handler
	server.ConnContext = func(ctx context.Context, _ net.Conn) context.Context {
		return context.WithValue(ctx, requestCounterKey{}, new(int))
	}
	server.Handler = http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		count := r.Context().Value(requestCounterKey{}).(*int)
		*count++
		if *count >= limit {
			w.Header().Set("Connection", "close")
		}
		handler.ServeHTTP(w, r)
	})
}

func serveHTTP(args []string) error {
	c, err := parseServer(args)
	if err != nil {
		return err
	}
	var handler http.Handler
	if c.mode == "static" {
		if c.root == "" {
			return errors.New("static root required")
		}
		handler = http.FileServer(http.Dir(c.root))
	} else if c.mode == "fixture" {
		handler = http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			if r.URL.Path == "/echo" && r.Method == "POST" {
				if err := http.NewResponseController(w).EnableFullDuplex(); err != nil {
					http.Error(w, "full duplex unavailable", http.StatusInternalServerError)
					return
				}
				w.Header().Set("Connection", "close")
				for name := range r.Trailer {
					w.Header().Add("Trailer", name)
				}
				w.WriteHeader(http.StatusOK)
				w.(http.Flusher).Flush()
				buffer := make([]byte, 16384)
				for {
					count, err := r.Body.Read(buffer)
					if count > 0 {
						if _, writeErr := w.Write(buffer[:count]); writeErr != nil {
							return
						}
						w.(http.Flusher).Flush()
					}
					if err == io.EOF {
						break
					}
					if err != nil {
						return
					}
				}
				for name, values := range r.Trailer {
					w.Header()[name] = values
				}
				return
			}
			if r.Method != "GET" {
				http.Error(w, "method", 405)
				return
			}
			if r.URL.Path == "/trailers" {
				w.Header().Set("Trailer", "X-Finished")
				w.WriteHeader(200)
				w.(http.Flusher).Flush()
				w.Write([]byte("first\n"))
				w.(http.Flusher).Flush()
				w.Write([]byte("second\n"))
				w.Header().Set("X-Finished", "yes")
				return
			}
			size, err := strconv.Atoi(strings.TrimPrefix(r.URL.Path, "/bytes/"))
			if !strings.HasPrefix(r.URL.Path, "/bytes/") || err != nil || size < 0 || size > 1<<20 {
				http.Error(w, "fixture", 404)
				return
			}
			w.Header().Set("Content-Length", strconv.Itoa(size))
			w.Header().Set("Content-Type", "application/octet-stream")
			w.Write(bytes.Repeat([]byte{'x'}, size))
		})
	} else {
		return errors.New("unknown HTTP reference mode")
	}
	return runServer(c, handler, nil)
}

type sharedMessage struct {
	data             []byte
	kind, references int
}
type recipient struct {
	conn         *websocket.Conn
	queue        chan *sharedMessage
	closed       bool
	bytes, count int
}
type broadcastHub struct {
	mu      sync.Mutex
	clients map[*recipient]bool
	bytes   int
	config  serverFlags
}

func (h *broadcastHub) release(p *recipient, msg *sharedMessage) {
	h.mu.Lock()
	defer h.mu.Unlock()
	p.bytes -= len(msg.data)
	p.count--
	msg.references--
	if msg.references == 0 {
		h.bytes -= len(msg.data)
	}
}
func (h *broadcastHub) removeLocked(p *recipient, code int) {
	if p.closed {
		return
	}
	p.closed = true
	delete(h.clients, p)
	close(p.queue)
	go func() {
		p.conn.WriteControl(websocket.CloseMessage, websocket.FormatCloseMessage(code, ""), time.Now().Add(h.config.closeTimeout))
		p.conn.Close()
	}()
}
func (h *broadcastHub) broadcast(kind int, body []byte) {
	h.mu.Lock()
	defer h.mu.Unlock()
	msg := &sharedMessage{data: body, kind: kind}
	if h.bytes+len(body) > h.config.totalBytes {
		for p := range h.clients {
			h.removeLocked(p, 1008)
		}
		return
	}
	for p := range h.clients {
		if p.count >= h.config.queue || p.bytes+len(body) > h.config.clientBytes {
			h.removeLocked(p, 1008)
			continue
		}
		p.count++
		p.bytes += len(body)
		msg.references++
		p.queue <- msg
	}
	if msg.references > 0 {
		h.bytes += len(body)
	}
}
func (h *broadcastHub) handle(w http.ResponseWriter, r *http.Request) {
	upgrader := websocket.Upgrader{CheckOrigin: func(*http.Request) bool { return true },
		EnableCompression: false, ReadBufferSize: 16384, WriteBufferSize: h.config.frameBytes}
	conn, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		return
	}
	p := &recipient{conn: conn, queue: make(chan *sharedMessage, h.config.queue)}
	h.mu.Lock()
	h.clients[p] = true
	h.mu.Unlock()
	go func() {
		for msg := range p.queue {
			h.mu.Lock()
			closed := p.closed
			h.mu.Unlock()
			var err error
			if !closed {
				conn.SetWriteDeadline(time.Now().Add(h.config.writeTimeout))
				writer, openErr := conn.NextWriter(msg.kind)
				err = openErr
				if err == nil {
					body := msg.data
					for len(body) > 0 && err == nil {
						size := min(len(body), h.config.frameBytes)
						_, err = writer.Write(body[:size])
						body = body[size:]
					}
					closeErr := writer.Close()
					if err == nil {
						err = closeErr
					}
				}
			}
			h.release(p, msg)
			if err != nil {
				h.mu.Lock()
				h.removeLocked(p, 1001)
				h.mu.Unlock()
			}
		}
	}()
	conn.SetReadLimit(int64(h.config.message))
	for {
		conn.SetReadDeadline(time.Now().Add(60 * time.Second))
		kind, body, err := conn.ReadMessage()
		if err != nil {
			break
		}
		if kind == websocket.TextMessage && !utf8.Valid(body) {
			h.mu.Lock()
			h.removeLocked(p, 1007)
			h.mu.Unlock()
			return
		}
		h.broadcast(kind, body)
	}
	h.mu.Lock()
	h.removeLocked(p, 1000)
	h.mu.Unlock()
}
func serveWS(args []string) error {
	c, err := parseServer(args)
	if err != nil {
		return err
	}
	hub := &broadcastHub{clients: make(map[*recipient]bool), config: c}
	return runServer(c, http.HandlerFunc(hub.handle), func() {
		hub.mu.Lock()
		defer hub.mu.Unlock()
		for p := range hub.clients {
			hub.removeLocked(p, 1001)
		}
	})
}
