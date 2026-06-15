// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

package main

import (
	"flag"
	"fmt"
	"io"
	"log"
	"net"
	"sync"
	"sync/atomic"
)

func main() {
	addr := flag.String("addr", "127.0.0.1:9000", "address to listen on")
	bufferSize := flag.Int("buffer-size", 16*1024, "per-connection read buffer size")
	maxConnections := flag.Uint64("max-connections", 0, "stop after accepting this many connections; 0 means unlimited")
	noDelay := flag.Bool("nodelay", true, "enable TCP_NODELAY on accepted sockets")
	flag.Parse()

	if *bufferSize <= 0 {
		log.Fatalf("-buffer-size must be greater than zero")
	}

	listener, err := net.Listen("tcp", *addr)
	if err != nil {
		log.Fatalf("listen failed: %v", err)
	}
	defer listener.Close()

	fmt.Printf("tcp_echo_host_go listening=%s buffer_size=%d nodelay=%t\n", listener.Addr(), *bufferSize, *noDelay)

	var accepted uint64
	var wg sync.WaitGroup
	for {
		if *maxConnections != 0 && atomic.LoadUint64(&accepted) >= *maxConnections {
			wg.Wait()
			return
		}

		conn, err := listener.Accept()
		if err != nil {
			log.Printf("accept failed: %v", err)
			continue
		}
		if tcpConn, ok := conn.(*net.TCPConn); ok {
			if err := tcpConn.SetNoDelay(*noDelay); err != nil {
				log.Printf("set TCP_NODELAY failed for %s: %v", conn.RemoteAddr(), err)
				_ = conn.Close()
				continue
			}
		}
		atomic.AddUint64(&accepted, 1)
		wg.Add(1)
		go func() {
			defer wg.Done()
			handleConnection(conn, *bufferSize)
		}()
	}
}

func handleConnection(conn net.Conn, bufferSize int) {
	defer conn.Close()

	buffer := make([]byte, bufferSize)
	for {
		n, err := conn.Read(buffer)
		if n > 0 {
			if _, writeErr := conn.Write(buffer[:n]); writeErr != nil {
				log.Printf("write failed for %s: %v", conn.RemoteAddr(), writeErr)
				return
			}
		}
		if err == nil {
			continue
		}
		if err != io.EOF {
			log.Printf("read failed for %s: %v", conn.RemoteAddr(), err)
		}
		return
	}
}
