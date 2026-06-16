// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

package main

import (
	"context"

	pb "github.com/Azure/kimojio-rs/examples/go/object_gateway/proto"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
)

func newObjectGatewayClient(ctx context.Context, addr string) (pb.ObjectGatewayClient, *grpc.ClientConn, error) {
	conn, err := grpc.NewClient(addr, grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		return nil, nil, err
	}
	return pb.NewObjectGatewayClient(conn), conn, nil
}

func clientHealth(ctx context.Context, addr string) (*pb.HealthResponse, error) {
	client, conn, err := newObjectGatewayClient(ctx, addr)
	if err != nil {
		return nil, err
	}
	defer conn.Close()
	return client.Health(ctx, &pb.HealthRequest{})
}

func clientPut(ctx context.Context, addr, namespace, object string, chunks [][]byte) (*pb.PutResponse, error) {
	client, conn, err := newObjectGatewayClient(ctx, addr)
	if err != nil {
		return nil, err
	}
	defer conn.Close()
	stream, err := client.Put(ctx)
	if err != nil {
		return nil, err
	}
	var offset uint64
	for index, chunk := range chunks {
		if err := stream.Send(&pb.PutChunk{
			Object:      key(namespace, object),
			Offset:      offset,
			Data:        chunk,
			Finish:      index == len(chunks)-1,
			ContentType: "application/octet-stream",
		}); err != nil {
			return nil, err
		}
		offset += uint64(len(chunk))
	}
	return stream.CloseAndRecv()
}
