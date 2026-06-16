#!/usr/bin/env sh
set -eu

tmp="${TMPDIR:-/tmp}/kimojio-object-gateway-protoc-$$"
rm -rf "$tmp"
mkdir -p "$tmp"
trap 'rm -rf "$tmp"' EXIT

GOBIN="$tmp" go install google.golang.org/protobuf/cmd/protoc-gen-go@v1.36.10
GOBIN="$tmp" go install google.golang.org/grpc/cmd/protoc-gen-go-grpc@v1.5.1

PATH="$tmp:$PATH" protoc \
  -I ../../proto \
  --go_out=proto \
  --go_opt=paths=source_relative \
  --go-grpc_out=proto \
  --go-grpc_opt=paths=source_relative \
  ../../proto/object_gateway.proto
