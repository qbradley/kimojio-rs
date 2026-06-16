// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"sort"
	"strings"
	"sync"
	"time"

	_ "github.com/Azure/azure-sdk-for-go/sdk/storage/azblob"
	pb "github.com/Azure/kimojio-rs/examples/go/object_gateway/proto"
	_ "go.opentelemetry.io/otel"
	_ "go.opentelemetry.io/otel/sdk"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

type objectKey struct {
	namespace string
	name      string
}

type storedObject struct {
	data       []byte
	metadata   *pb.ObjectMetadata
	generation uint64
}

type failureClass int

const (
	failureTimeout failureClass = iota + 1
	failureRetriable
	failureNonRetriable
)

type telemetryRecord struct {
	operation string
	code      pb.ObjectStatusCode
}

type adminStatus struct {
	ready        bool
	shuttingDown bool
	inFlight     uint64
	completed    uint64
	failed       uint64
	version      string
}

type gatewayState struct {
	mu             sync.Mutex
	objects        map[objectKey]storedObject
	failures       []failureClass
	telemetry      []telemetryRecord
	telemetryFails []string
	admin          adminStatus
	maxObjectBytes uint64
	maxChunkBytes  int
}

func newGatewayState(version string, maxObjectBytes uint64, maxChunkBytes int) *gatewayState {
	return &gatewayState{
		objects:        make(map[objectKey]storedObject),
		admin:          adminStatus{ready: true, version: version},
		maxObjectBytes: maxObjectBytes,
		maxChunkBytes:  maxChunkBytes,
	}
}

func (s *gatewayState) injectFailure(failure failureClass) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.failures = append(s.failures, failure)
}

func (s *gatewayState) failNextTelemetry(message string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.telemetryFails = append(s.telemetryFails, message)
}

func (s *gatewayState) snapshotTelemetry() []telemetryRecord {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := make([]telemetryRecord, len(s.telemetry))
	copy(out, s.telemetry)
	return out
}

func (s *gatewayState) beginShutdown() {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.admin.ready = false
	s.admin.shuttingDown = true
}

func (s *gatewayState) health() *pb.HealthResponse {
	s.mu.Lock()
	defer s.mu.Unlock()
	return &pb.HealthResponse{
		Ready:        s.admin.ready,
		ShuttingDown: s.admin.shuttingDown,
		InFlight:     s.admin.inFlight,
		Completed:    s.admin.completed,
		Failed:       s.admin.failed,
		Version:      s.admin.version,
	}
}

func (s *gatewayState) record(operation string, status *pb.OperationStatus) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.telemetry = append(s.telemetry, telemetryRecord{operation: operation, code: status.Code})
	if len(s.telemetryFails) > 0 {
		s.telemetryFails = s.telemetryFails[1:]
	}
	if status.Code == pb.ObjectStatusCode_OBJECT_STATUS_OK {
		s.admin.completed++
	} else {
		s.admin.failed++
	}
}

func (s *gatewayState) popFailureLocked() (failureClass, bool) {
	if len(s.failures) == 0 {
		return 0, false
	}
	failure := s.failures[0]
	s.failures = s.failures[1:]
	return failure, true
}

func (s *gatewayState) put(key objectKey, contentType string, body []byte) (*pb.PutResponse, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if failure, ok := s.popFailureLocked(); ok {
		return &pb.PutResponse{Status: statusForFailure(failure)}, nil
	}
	s.admin.inFlight++
	defer func() { s.admin.inFlight-- }()
	if uint64(len(body)) > s.maxObjectBytes {
		return &pb.PutResponse{Status: objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_SIZE_LIMIT)}, nil
	}
	generation := uint64(1)
	if existing, ok := s.objects[key]; ok {
		generation = existing.generation + 1
	}
	data := append([]byte(nil), body...)
	metadata := &pb.ObjectMetadata{
		Object:       &pb.ObjectIdentity{Namespace: key.namespace, Name: key.name},
		SizeBytes:    uint64(len(data)),
		Generation:   generation,
		Etag:         fmt.Sprintf("etag-%d", generation),
		ContentType:  contentType,
		UserMetadata: map[string]string{},
	}
	s.objects[key] = storedObject{data: data, metadata: metadata, generation: generation}
	return &pb.PutResponse{Status: objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_OK), Metadata: metadata}, nil
}

func (s *gatewayState) get(key objectKey) (*storedObject, *pb.OperationStatus) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if failure, ok := s.popFailureLocked(); ok {
		return nil, statusForFailure(failure)
	}
	s.admin.inFlight++
	defer func() { s.admin.inFlight-- }()
	object, ok := s.objects[key]
	if !ok {
		return nil, objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_NOT_FOUND)
	}
	data := append([]byte(nil), object.data...)
	metadata := *object.metadata
	return &storedObject{data: data, metadata: &metadata, generation: object.generation}, objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_OK)
}

func (s *gatewayState) delete(key objectKey) *pb.DeleteResponse {
	s.mu.Lock()
	defer s.mu.Unlock()
	if failure, ok := s.popFailureLocked(); ok {
		return &pb.DeleteResponse{Status: statusForFailure(failure)}
	}
	s.admin.inFlight++
	defer func() { s.admin.inFlight-- }()
	if _, ok := s.objects[key]; !ok {
		return &pb.DeleteResponse{Status: objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_NOT_FOUND)}
	}
	delete(s.objects, key)
	return &pb.DeleteResponse{Status: objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_OK)}
}

func (s *gatewayState) list(namespace, prefix string, maxResults uint32) *pb.ListResponse {
	s.mu.Lock()
	defer s.mu.Unlock()
	if failure, ok := s.popFailureLocked(); ok {
		return &pb.ListResponse{Status: statusForFailure(failure)}
	}
	s.admin.inFlight++
	defer func() { s.admin.inFlight-- }()
	keys := make([]objectKey, 0, len(s.objects))
	for key := range s.objects {
		if key.namespace == namespace && strings.HasPrefix(key.name, prefix) {
			keys = append(keys, key)
		}
	}
	sort.Slice(keys, func(i, j int) bool { return keys[i].name < keys[j].name })
	limit := len(keys)
	if maxResults > 0 && int(maxResults) < limit {
		limit = int(maxResults)
	}
	objects := make([]*pb.ObjectMetadata, 0, limit)
	for _, key := range keys {
		if maxResults > 0 && len(objects) >= int(maxResults) {
			break
		}
		metadata := *s.objects[key].metadata
		objects = append(objects, &metadata)
	}
	return &pb.ListResponse{Status: objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_OK), Objects: objects}
}

func (s *gatewayState) copy(source, destination objectKey) *pb.CopyResponse {
	s.mu.Lock()
	defer s.mu.Unlock()
	if failure, ok := s.popFailureLocked(); ok {
		return &pb.CopyResponse{Status: statusForFailure(failure)}
	}
	s.admin.inFlight++
	defer func() { s.admin.inFlight-- }()
	object, ok := s.objects[source]
	if !ok {
		return &pb.CopyResponse{Status: objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_NOT_FOUND)}
	}
	generation := object.generation + 1
	data := append([]byte(nil), object.data...)
	metadata := &pb.ObjectMetadata{
		Object:       &pb.ObjectIdentity{Namespace: destination.namespace, Name: destination.name},
		SizeBytes:    uint64(len(data)),
		Generation:   generation,
		Etag:         fmt.Sprintf("etag-%d", generation),
		ContentType:  object.metadata.ContentType,
		UserMetadata: map[string]string{},
	}
	s.objects[destination] = storedObject{data: data, metadata: metadata, generation: generation}
	return &pb.CopyResponse{Status: objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_OK), Metadata: metadata}
}

type gatewayServer struct {
	pb.UnimplementedObjectGatewayServer
	state *gatewayState
}

func (s *gatewayServer) Put(stream pb.ObjectGateway_PutServer) error {
	var key objectKey
	var body []byte
	contentType := "application/octet-stream"
	first := true
	var nextOffset uint64
	finished := false
	for {
		chunk, err := stream.Recv()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return err
		}
		if finished {
			return stream.SendAndClose(&pb.PutResponse{Status: objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_INTERNAL)})
		}
		if chunk.Object == nil || chunk.Object.Namespace == "" || chunk.Object.Name == "" {
			return stream.SendAndClose(&pb.PutResponse{Status: objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_INTERNAL)})
		}
		chunkKey := objectKey{namespace: chunk.Object.Namespace, name: chunk.Object.Name}
		if first {
			key = chunkKey
			first = false
			if chunk.ContentType != "" {
				contentType = chunk.ContentType
			}
		} else if chunkKey != key {
			return stream.SendAndClose(&pb.PutResponse{Status: objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_INTERNAL)})
		}
		if chunk.Offset != nextOffset {
			return stream.SendAndClose(&pb.PutResponse{Status: objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_INTERNAL)})
		}
		if len(chunk.Data) > s.state.maxChunkBytes || uint64(len(body)+len(chunk.Data)) > s.state.maxObjectBytes {
			for {
				_, err := stream.Recv()
				if errors.Is(err, io.EOF) {
					break
				}
				if err != nil {
					return err
				}
			}
			response := &pb.PutResponse{Status: objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_SIZE_LIMIT)}
			s.state.record("put", response.Status)
			return stream.SendAndClose(response)
		}
		body = append(body, chunk.Data...)
		nextOffset += uint64(len(chunk.Data))
		finished = chunk.Finish
	}
	if first {
		return stream.SendAndClose(&pb.PutResponse{Status: objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_INTERNAL)})
	}
	response, err := s.state.put(key, contentType, body)
	if err != nil {
		return err
	}
	s.state.record("put", response.Status)
	return stream.SendAndClose(response)
}

func (s *gatewayServer) Get(request *pb.GetRequest, stream pb.ObjectGateway_GetServer) error {
	if request.Object == nil {
		return stream.Send(&pb.GetChunk{Status: objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_INTERNAL), Finish: true})
	}
	maxChunkBytes := int(request.MaxChunkBytes)
	if maxChunkBytes == 0 {
		maxChunkBytes = s.state.maxChunkBytes
	}
	key := objectKey{namespace: request.Object.Namespace, name: request.Object.Name}
	if maxChunkBytes <= 0 || maxChunkBytes > s.state.maxChunkBytes {
		status := objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_SIZE_LIMIT)
		s.state.record("get", status)
		return stream.Send(&pb.GetChunk{Status: status, Finish: true})
	}
	object, status := s.state.get(key)
	s.state.record("get", status)
	if status.Code != pb.ObjectStatusCode_OBJECT_STATUS_OK {
		return stream.Send(&pb.GetChunk{Status: status, Finish: true})
	}
	if len(object.data) == 0 {
		return stream.Send(&pb.GetChunk{Status: status, Metadata: object.metadata, Finish: true})
	}
	for offset := 0; offset < len(object.data); offset += maxChunkBytes {
		end := offset + maxChunkBytes
		if end > len(object.data) {
			end = len(object.data)
		}
		if err := stream.Send(&pb.GetChunk{
			Status:   status,
			Metadata: object.metadata,
			Offset:   uint64(offset),
			Data:     object.data[offset:end],
			Finish:   end == len(object.data),
		}); err != nil {
			return err
		}
	}
	return nil
}

func (s *gatewayServer) Delete(ctx context.Context, request *pb.DeleteRequest) (*pb.DeleteResponse, error) {
	if request.Object == nil {
		return &pb.DeleteResponse{Status: objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_INTERNAL)}, nil
	}
	response := s.state.delete(objectKey{namespace: request.Object.Namespace, name: request.Object.Name})
	s.state.record("delete", response.Status)
	return response, nil
}

func (s *gatewayServer) List(ctx context.Context, request *pb.ListRequest) (*pb.ListResponse, error) {
	response := s.state.list(request.Namespace, request.Prefix, request.MaxResults)
	s.state.record("list", response.Status)
	return response, nil
}

func (s *gatewayServer) Copy(ctx context.Context, request *pb.CopyRequest) (*pb.CopyResponse, error) {
	if request.Source == nil || request.Destination == nil {
		return &pb.CopyResponse{Status: objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_INTERNAL)}, nil
	}
	response := s.state.copy(
		objectKey{namespace: request.Source.Namespace, name: request.Source.Name},
		objectKey{namespace: request.Destination.Namespace, name: request.Destination.Name},
	)
	s.state.record("copy", response.Status)
	return response, nil
}

func (s *gatewayServer) Health(ctx context.Context, request *pb.HealthRequest) (*pb.HealthResponse, error) {
	health := s.state.health()
	s.state.record("health", objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_OK))
	return health, nil
}

func main() {
	var grpcAddr string
	var adminAddr string
	var workers int
	var maxObjectBytes uint64
	var maxChunkBytes int
	var requestDeadlineMS int
	var shutdownAfterReady bool
	flag.StringVar(&grpcAddr, "grpc-addr", "127.0.0.1:9300", "gRPC listen address")
	flag.StringVar(&adminAddr, "admin-addr", "127.0.0.1:9301", "admin HTTP listen address")
	flag.IntVar(&workers, "workers", 0, "reported goroutine worker setting")
	flag.Uint64Var(&maxObjectBytes, "max-object-bytes", 4*1024*1024, "maximum object bytes")
	flag.IntVar(&maxChunkBytes, "max-chunk-bytes", 64*1024, "maximum chunk bytes")
	flag.IntVar(&requestDeadlineMS, "request-deadline-ms", 30000, "request deadline in milliseconds")
	flag.BoolVar(&shutdownAfterReady, "shutdown-after-ready", false, "run readiness self-check and exit")
	flag.Parse()
	if maxObjectBytes == 0 || maxChunkBytes <= 0 || requestDeadlineMS <= 0 {
		log.Fatal("size limits and deadline must be positive")
	}
	state := newGatewayState("0.1.0", maxObjectBytes, maxChunkBytes)
	if err := runHost(grpcAddr, adminAddr, workers, time.Duration(requestDeadlineMS)*time.Millisecond, shutdownAfterReady, state); err != nil {
		log.Fatal(err)
	}
}

func runHost(grpcAddr, adminAddr string, workers int, deadline time.Duration, shutdownAfterReady bool, state *gatewayState) error {
	grpcListener, err := net.Listen("tcp", grpcAddr)
	if err != nil {
		return err
	}
	adminListener, err := net.Listen("tcp", adminAddr)
	if err != nil {
		grpcListener.Close()
		return err
	}
	grpcServer := grpc.NewServer(
		grpc.ConnectionTimeout(deadline),
		grpc.UnaryInterceptor(deadlineUnaryInterceptor(deadline)),
		grpc.StreamInterceptor(deadlineStreamInterceptor(deadline)),
	)
	pb.RegisterObjectGatewayServer(grpcServer, &gatewayServer{state: state})
	adminServer := &http.Server{Handler: adminHandler(state), ReadHeaderTimeout: deadline}
	fmt.Printf("object-gateway-go-host runtime=go runtime_mode=goroutine grpc_addr=%s admin_addr=%s workers=%d storage_client=hermetic-storage-fixture storage_backend=hermetic telemetry_client=hermetic-otlp-fixture telemetry_sink=hermetic ready=%t deadline_ms=%d\n",
		grpcListener.Addr(), adminListener.Addr(), workers, state.health().Ready, deadline.Milliseconds())
	grpcErr := make(chan error, 1)
	adminErr := make(chan error, 1)
	go func() { grpcErr <- grpcServer.Serve(grpcListener) }()
	go func() { adminErr <- adminServer.Serve(adminListener) }()

	if shutdownAfterReady {
		if err := selfCheck(grpcListener.Addr().String(), adminListener.Addr().String(), state); err != nil {
			return err
		}
		grpcServer.GracefulStop()
		ctx, cancel := context.WithTimeout(context.Background(), deadline)
		defer cancel()
		return adminServer.Shutdown(ctx)
	}
	select {
	case err := <-grpcErr:
		return err
	case err := <-adminErr:
		if errors.Is(err, http.ErrServerClosed) {
			return nil
		}
		return err
	}
}

func deadlineUnaryInterceptor(deadline time.Duration) grpc.UnaryServerInterceptor {
	return func(ctx context.Context, req any, info *grpc.UnaryServerInfo, handler grpc.UnaryHandler) (any, error) {
		ctx, cancel := context.WithTimeout(ctx, deadline)
		defer cancel()
		return handler(ctx, req)
	}
}

type deadlineServerStream struct {
	grpc.ServerStream
	ctx context.Context
}

func (s *deadlineServerStream) Context() context.Context {
	return s.ctx
}

func (s *deadlineServerStream) RecvMsg(m any) error {
	errs := make(chan error, 1)
	go func() { errs <- s.ServerStream.RecvMsg(m) }()
	select {
	case err := <-errs:
		return err
	case <-s.ctx.Done():
		return status.Error(codes.DeadlineExceeded, "request deadline exceeded")
	}
}

func (s *deadlineServerStream) SendMsg(m any) error {
	errs := make(chan error, 1)
	go func() { errs <- s.ServerStream.SendMsg(m) }()
	select {
	case err := <-errs:
		return err
	case <-s.ctx.Done():
		return status.Error(codes.DeadlineExceeded, "request deadline exceeded")
	}
}

func deadlineStreamInterceptor(deadline time.Duration) grpc.StreamServerInterceptor {
	return func(srv any, stream grpc.ServerStream, info *grpc.StreamServerInfo, handler grpc.StreamHandler) error {
		ctx, cancel := context.WithTimeout(stream.Context(), deadline)
		defer cancel()
		return handler(srv, &deadlineServerStream{ServerStream: stream, ctx: ctx})
	}
}

func adminHandler(state *gatewayState) http.Handler {
	mux := http.NewServeMux()
	handler := func(w http.ResponseWriter, r *http.Request) {
		status := state.health()
		w.Header().Set("content-type", "text/plain")
		fmt.Fprintf(w, "ready=%t shutting_down=%t in_flight=%d completed=%d failed=%d version=%s\n",
			status.Ready, status.ShuttingDown, status.InFlight, status.Completed, status.Failed, status.Version)
	}
	mux.HandleFunc("/health", handler)
	mux.HandleFunc("/status", handler)
	return mux
}

func selfCheck(grpcAddr, adminAddr string, state *gatewayState) error {
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	health, err := clientHealth(ctx, grpcAddr)
	if err != nil {
		return err
	}
	if !health.Ready {
		return fmt.Errorf("gRPC health not ready")
	}
	body, err := httpGet(ctx, "http://"+adminAddr+"/health")
	if err != nil {
		return err
	}
	if !strings.Contains(body, "ready=true") {
		return fmt.Errorf("admin health not ready: %s", body)
	}
	state.beginShutdown()
	body, err = httpGet(ctx, "http://"+adminAddr+"/status")
	if err != nil {
		return err
	}
	if !strings.Contains(body, "shutting_down=true") {
		return fmt.Errorf("admin status missing shutdown: %s", body)
	}
	return nil
}

func httpGet(ctx context.Context, url string) (string, error) {
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return "", err
	}
	response, err := http.DefaultClient.Do(request)
	if err != nil {
		return "", err
	}
	defer response.Body.Close()
	body, err := io.ReadAll(response.Body)
	return string(body), err
}

func key(namespace, name string) *pb.ObjectIdentity {
	return &pb.ObjectIdentity{Namespace: namespace, Name: name}
}

func objectStatus(code pb.ObjectStatusCode) *pb.OperationStatus {
	status := &pb.OperationStatus{Code: code, StorageClass: pb.StorageStatusClass_STORAGE_STATUS_NONE}
	switch code {
	case pb.ObjectStatusCode_OBJECT_STATUS_NOT_FOUND:
		status.Message = "object not found"
	case pb.ObjectStatusCode_OBJECT_STATUS_SIZE_LIMIT:
		status.Message = "object size limit exceeded"
	case pb.ObjectStatusCode_OBJECT_STATUS_DEADLINE_EXCEEDED:
		status.Message = "deadline exceeded"
	case pb.ObjectStatusCode_OBJECT_STATUS_CANCELLED:
		status.Message = "cancelled"
	case pb.ObjectStatusCode_OBJECT_STATUS_INTERNAL:
		status.Message = "internal error"
	}
	return status
}

func statusForFailure(failure failureClass) *pb.OperationStatus {
	switch failure {
	case failureTimeout:
		return &pb.OperationStatus{Code: pb.ObjectStatusCode_OBJECT_STATUS_STORAGE_TIMEOUT, Message: "storage timeout", StorageClass: pb.StorageStatusClass_STORAGE_STATUS_TIMEOUT}
	case failureRetriable:
		return &pb.OperationStatus{Code: pb.ObjectStatusCode_OBJECT_STATUS_STORAGE_RETRIABLE, Message: "storage retriable failure", StorageClass: pb.StorageStatusClass_STORAGE_STATUS_RETRIABLE}
	case failureNonRetriable:
		return &pb.OperationStatus{Code: pb.ObjectStatusCode_OBJECT_STATUS_STORAGE_NON_RETRIABLE, Message: "storage non-retriable failure", StorageClass: pb.StorageStatusClass_STORAGE_STATUS_NON_RETRIABLE}
	default:
		return objectStatus(pb.ObjectStatusCode_OBJECT_STATUS_INTERNAL)
	}
}
