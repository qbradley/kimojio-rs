// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

package main

import (
	"context"
	"errors"
	"io"
	"net"
	"testing"
	"time"

	pb "github.com/Azure/kimojio-rs/examples/go/object_gateway/proto"
	"google.golang.org/grpc"
)

func TestObjectGatewayGoPutGetTelemetry(t *testing.T) {
	state := newGatewayState("test", 16, 4)
	addr, stop := startTestGRPC(t, state)
	defer stop()

	response, err := clientPut(context.Background(), addr, "go", "object", [][]byte{[]byte("ab"), []byte("cd")})
	if err != nil {
		t.Fatal(err)
	}
	if response.Status.Code != pb.ObjectStatusCode_OBJECT_STATUS_OK {
		t.Fatalf("put status = %v", response.Status.Code)
	}

	client, conn, err := newObjectGatewayClient(context.Background(), addr)
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	stream, err := client.Get(context.Background(), &pb.GetRequest{Object: key("go", "object"), MaxChunkBytes: 2})
	if err != nil {
		t.Fatal(err)
	}
	var body []byte
	for {
		chunk, err := stream.Recv()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			t.Fatal(err)
		}
		if len(chunk.Data) > 2 {
			t.Fatalf("chunk too large: %d", len(chunk.Data))
		}
		body = append(body, chunk.Data...)
	}
	if string(body) != "abcd" {
		t.Fatalf("body = %q", body)
	}
	if len(state.snapshotTelemetry()) < 2 {
		t.Fatalf("expected telemetry records")
	}
}

func TestObjectGatewayGoSizeLimitAndStorageFailures(t *testing.T) {
	state := newGatewayState("test", 4, 2)
	addr, stop := startTestGRPC(t, state)
	defer stop()

	response, err := clientPut(context.Background(), addr, "go", "too-large", [][]byte{[]byte("abc")})
	if err != nil {
		t.Fatal(err)
	}
	if response.Status.Code != pb.ObjectStatusCode_OBJECT_STATUS_SIZE_LIMIT {
		t.Fatalf("size status = %v", response.Status.Code)
	}

	state.injectFailure(failureTimeout)
	client, conn, err := newObjectGatewayClient(context.Background(), addr)
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	stream, err := client.Get(context.Background(), &pb.GetRequest{Object: key("go", "missing"), MaxChunkBytes: 2})
	if err != nil {
		t.Fatal(err)
	}
	chunk, err := stream.Recv()
	if err != nil {
		t.Fatal(err)
	}
	if chunk.Status.Code != pb.ObjectStatusCode_OBJECT_STATUS_STORAGE_TIMEOUT {
		t.Fatalf("storage status = %v", chunk.Status.Code)
	}
}

func TestObjectGatewayGoCancellationAndShutdown(t *testing.T) {
	state := newGatewayState("test", 16, 1)
	addr, stop := startTestGRPC(t, state)
	defer stop()
	if _, err := clientPut(context.Background(), addr, "go", "cancel", [][]byte{[]byte("abcd")}); err != nil {
		t.Fatal(err)
	}

	ctx, cancel := context.WithCancel(context.Background())
	client, conn, err := newObjectGatewayClient(ctx, addr)
	if err != nil {
		t.Fatal(err)
	}
	stream, err := client.Get(ctx, &pb.GetRequest{Object: key("go", "cancel"), MaxChunkBytes: 1})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := stream.Recv(); err != nil {
		t.Fatal(err)
	}
	cancel()
	conn.Close()

	shutdownState := newGatewayState("test", 16, 4)
	if err := runHost("127.0.0.1:0", "127.0.0.1:0", 0, time.Second, true, shutdownState); err != nil {
		t.Fatal(err)
	}
	if !shutdownState.health().ShuttingDown {
		t.Fatalf("expected shutdown state")
	}
}

func startTestGRPC(t *testing.T, state *gatewayState) (string, func()) {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	server := grpc.NewServer()
	pb.RegisterObjectGatewayServer(server, &gatewayServer{state: state})
	errs := make(chan error, 1)
	go func() { errs <- server.Serve(listener) }()
	return listener.Addr().String(), func() {
		server.GracefulStop()
		if err := <-errs; err != nil {
			t.Fatal(err)
		}
	}
}
