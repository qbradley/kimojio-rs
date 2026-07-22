#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# End-to-end test suite for the static-file-server example.
#
# Exercises the server with curl across all four transport/protocol
# combinations (cleartext HTTP/1.1, cleartext HTTP/2 with prior knowledge,
# TLS HTTP/1.1, and TLS HTTP/2 negotiated via ALPN).
#
# The suite builds its own document root, generates a throwaway self-signed
# certificate, binds ephemeral ports, and cleans everything up on exit.
#
# Usage: examples/tests/static_file_server_curl.sh [path/to/static-file-server]

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SERVER_BIN="${1:-}"

PASS_COUNT=0
FAIL_COUNT=0
FAILURES=()
WORK_DIR=""
SERVER_PID=""

log() { printf '%s\n' "$*"; }

pass() {
    PASS_COUNT=$((PASS_COUNT + 1))
    printf 'ok   %s\n' "$1"
}

fail() {
    FAIL_COUNT=$((FAIL_COUNT + 1))
    FAILURES+=("$1")
    printf 'FAIL %s\n' "$1"
    if [[ $# -gt 1 ]]; then
        printf '       expected: %s\n' "$2"
        printf '       actual:   %s\n' "${3-}"
    fi
}

# check <name> <expected> <actual>
check() {
    if [[ "$2" == "$3" ]]; then
        pass "$1"
    else
        fail "$1" "$2" "$3"
    fi
}

# check_contains <name> <needle> <haystack>
check_contains() {
    if [[ "$3" == *"$2"* ]]; then
        pass "$1"
    else
        fail "$1" "output containing '$2'" "$(printf '%s' "$3" | head -c 200)"
    fi
}

cleanup() {
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null
        for _ in $(seq 1 50); do
            kill -0 "$SERVER_PID" 2>/dev/null || break
            sleep 0.1
        done
        kill -9 "$SERVER_PID" 2>/dev/null
        wait "$SERVER_PID" 2>/dev/null
    fi
    [[ -n "$WORK_DIR" && -d "$WORK_DIR" ]] && rm -rf "$WORK_DIR"
    return 0
}
trap cleanup EXIT INT TERM

require() {
    command -v "$1" >/dev/null 2>&1 || {
        log "SKIP: required tool '$1' not found"
        exit 0
    }
}

require curl
require openssl

if ! curl --version | head -1 | grep -q .; then
    log "SKIP: curl unusable"
    exit 0
fi

if ! curl --version | grep -qi 'HTTP2'; then
    log "SKIP: curl lacks HTTP/2 support"
    exit 0
fi

# ---------------------------------------------------------------------------
# Build the server binary if one was not supplied.
# ---------------------------------------------------------------------------

if [[ -z "$SERVER_BIN" ]]; then
    log "building static-file-server..."
    if ! cargo build --quiet --manifest-path "$REPO_ROOT/Cargo.toml" \
        -p examples --bin static-file-server --features http,tls; then
        log "SKIP: unable to build static-file-server"
        exit 0
    fi
    SERVER_BIN="$REPO_ROOT/target/debug/static-file-server"
fi

if [[ ! -x "$SERVER_BIN" ]]; then
    log "SKIP: server binary '$SERVER_BIN' is not executable"
    exit 0
fi

# ---------------------------------------------------------------------------
# Build the document root and TLS material.
# ---------------------------------------------------------------------------

WORK_DIR="$(mktemp -d)"
ROOT="$WORK_DIR/root"
mkdir -p "$ROOT/nested" "$ROOT/emptydir" "$ROOT/dir with space"

printf '<!doctype html><h1>index</h1>' >"$ROOT/index.html"
printf '<!doctype html><p>page</p>' >"$ROOT/page.htm"
printf 'body { color: red; }' >"$ROOT/style.css"
printf 'export const x = 1;' >"$ROOT/app.js"
printf 'export const y = 2;' >"$ROOT/mod.mjs"
printf '{"ok":true}' >"$ROOT/data.json"
printf 'plain text body' >"$ROOT/notes.txt"
printf '<svg xmlns="http://www.w3.org/2000/svg"/>' >"$ROOT/icon.svg"
printf '<?xml version="1.0"?><r/>' >"$ROOT/feed.xml"
printf 'PNGDATA' >"$ROOT/pic.png"
printf 'JPEGDATA' >"$ROOT/photo.jpg"
printf 'JPEGDATA2' >"$ROOT/photo2.jpeg"
printf 'GIFDATA' >"$ROOT/anim.gif"
printf 'ICODATA' >"$ROOT/favicon.ico"
printf '\0asm' >"$ROOT/mod.wasm"
printf '%%PDF-1.4' >"$ROOT/doc.pdf"
printf 'no extension here' >"$ROOT/README"
printf 'unknown ext' >"$ROOT/blob.zzz"
: >"$ROOT/empty.txt"
printf 'nested body' >"$ROOT/nested/deep.txt"
printf '<h1>nested index</h1>' >"$ROOT/nested/index.html"
printf 'spaced file' >"$ROOT/dir with space/file name.txt"

# A file larger than the HTTP/2 initial flow-control window (65535 bytes)
# so DATA framing and WINDOW_UPDATE handling are exercised.
LARGE_SIZE=200000
head -c "$LARGE_SIZE" /dev/urandom >"$ROOT/large.bin"

# A secret placed outside the document root; traversal tests must never reach it.
printf 'TOP SECRET' >"$WORK_DIR/secret.txt"
ln -s "$WORK_DIR/secret.txt" "$ROOT/escape-link.txt"

openssl req -x509 -newkey rsa:2048 \
    -keyout "$WORK_DIR/key.pem" -out "$WORK_DIR/cert.pem" \
    -days 1 -nodes -subj "/CN=localhost" \
    -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" >/dev/null 2>&1 || {
    log "SKIP: unable to generate a self-signed certificate"
    exit 0
}

# ---------------------------------------------------------------------------
# Start the server on ephemeral ports and discover them from the readiness lines.
# ---------------------------------------------------------------------------

SERVER_LOG="$WORK_DIR/server.log"
"$SERVER_BIN" \
    --root "$ROOT" \
    --listen 127.0.0.1:0 \
    --tls-listen 127.0.0.1:0 \
    --cert "$WORK_DIR/cert.pem" \
    --key "$WORK_DIR/key.pem" \
    >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!

PLAIN_URL=""
TLS_URL=""
for _ in $(seq 1 150); do
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        log "server exited during startup:"
        cat "$SERVER_LOG"
        exit 1
    fi
    PLAIN_URL="$(sed -n 's/^listening plain //p' "$SERVER_LOG" | head -1)"
    TLS_URL="$(sed -n 's/^listening tls //p' "$SERVER_LOG" | head -1)"
    [[ -n "$PLAIN_URL" && -n "$TLS_URL" ]] && break
    sleep 0.1
done

if [[ -z "$PLAIN_URL" || -z "$TLS_URL" ]]; then
    log "server did not report readiness in time:"
    cat "$SERVER_LOG"
    exit 1
fi

log "cleartext base: $PLAIN_URL"
log "tls base:       $TLS_URL"
log ""

# ---------------------------------------------------------------------------
# curl helpers, one per transport/protocol combination.
# ---------------------------------------------------------------------------

# Common flags: fail fast on connection problems but still report HTTP status.
CURL_COMMON=(--silent --show-error --max-time 30 --path-as-is)
TLS_FLAGS=(--insecure)

# request <combo> <path> [extra curl args...] -> writes body to stdout
# Sets REQ_STATUS, REQ_HEADERS, REQ_SIZE, REQ_PROTO.
REQ_STATUS=""
REQ_HEADERS=""
REQ_SIZE=""
REQ_PROTO=""
REQ_BODY_FILE=""

request() {
    local combo="$1" path="$2"
    shift 2
    local base proto_flags=()
    case "$combo" in
    h1) base="$PLAIN_URL" proto_flags=(--http1.1) ;;
    h2c) base="$PLAIN_URL" proto_flags=(--http2-prior-knowledge) ;;
    tls-h1) base="$TLS_URL" proto_flags=(--http1.1 "${TLS_FLAGS[@]}") ;;
    tls-h2) base="$TLS_URL" proto_flags=(--http2 "${TLS_FLAGS[@]}") ;;
    *)
        fail "internal: unknown combo $combo"
        return 1
        ;;
    esac

    REQ_BODY_FILE="$WORK_DIR/body.out"
    local header_file="$WORK_DIR/head.out"
    local metrics
    metrics="$(curl "${CURL_COMMON[@]}" "${proto_flags[@]}" "$@" \
        --dump-header "$header_file" \
        --output "$REQ_BODY_FILE" \
        --write-out '%{http_code} %{size_download} %{http_version}' \
        "${base}${path}" 2>"$WORK_DIR/curl.err")"
    local rc=$?
    if [[ $rc -ne 0 ]]; then
        REQ_STATUS="curl-error-$rc"
        REQ_HEADERS="$(cat "$WORK_DIR/curl.err")"
        REQ_SIZE=""
        REQ_PROTO=""
        return 0
    fi
    REQ_STATUS="${metrics%% *}"
    local rest="${metrics#* }"
    REQ_SIZE="${rest%% *}"
    REQ_PROTO="${rest##* }"
    # Normalize header names to lowercase for protocol-independent assertions.
    REQ_HEADERS="$(tr 'A-Z' 'a-z' <"$header_file")"
    return 0
}

ALL_COMBOS=(h1 h2c tls-h1 tls-h2)

# ---------------------------------------------------------------------------
# 1. Basic retrieval across every combination.
# ---------------------------------------------------------------------------

log "== basic retrieval =="
for combo in "${ALL_COMBOS[@]}"; do
    request "$combo" "/notes.txt"
    check "$combo GET /notes.txt status" "200" "$REQ_STATUS"
    check "$combo GET /notes.txt body" "plain text body" "$(cat "$REQ_BODY_FILE")"
    check_contains "$combo GET /notes.txt content-type" \
        "content-type: text/plain; charset=utf-8" "$REQ_HEADERS"
    check_contains "$combo GET /notes.txt content-length" \
        "content-length: 15" "$REQ_HEADERS"
done

# ---------------------------------------------------------------------------
# 2. Negotiated protocol is the one we asked for.
# ---------------------------------------------------------------------------

log ""
log "== protocol negotiation =="
request h1 "/notes.txt"
check "cleartext http/1.1 negotiated" "1.1" "$REQ_PROTO"
request h2c "/notes.txt"
check "cleartext h2 prior-knowledge negotiated" "2" "$REQ_PROTO"
request tls-h1 "/notes.txt"
check "tls http/1.1 negotiated" "1.1" "$REQ_PROTO"
request tls-h2 "/notes.txt"
check "tls h2 negotiated via alpn" "2" "$REQ_PROTO"

# ALPN must actually advertise h2 on the wire.
alpn_out="$(printf 'Q' | openssl s_client -connect "${TLS_URL#https://}" \
    -alpn h2 -servername localhost 2>&1 </dev/null)"
check_contains "openssl reports ALPN protocol h2" "ALPN protocol: h2" "$alpn_out"

alpn_http1="$(printf 'Q' | openssl s_client -connect "${TLS_URL#https://}" \
    -alpn http/1.1 -servername localhost 2>&1 </dev/null)"
check_contains "openssl reports ALPN protocol http/1.1" \
    "ALPN protocol: http/1.1" "$alpn_http1"

# ---------------------------------------------------------------------------
# 3. HEAD requests: status and headers match GET, but no body.
# ---------------------------------------------------------------------------

log ""
log "== HEAD requests =="
for combo in "${ALL_COMBOS[@]}"; do
    request "$combo" "/notes.txt" --head
    check "$combo HEAD status" "200" "$REQ_STATUS"
    check "$combo HEAD sends no body" "0" "$REQ_SIZE"
    check_contains "$combo HEAD keeps content-length" "content-length: 15" "$REQ_HEADERS"
    check_contains "$combo HEAD keeps content-type" \
        "content-type: text/plain; charset=utf-8" "$REQ_HEADERS"
done

# ---------------------------------------------------------------------------
# 4. Content-type mapping.
# ---------------------------------------------------------------------------

log ""
log "== content types =="
check_content_type() {
    local path="$1" expected="$2"
    request h1 "$path"
    check "content-type $path" "200" "$REQ_STATUS"
    check_contains "content-type $path => $expected" "content-type: $expected" "$REQ_HEADERS"
}

check_content_type "/index.html" "text/html; charset=utf-8"
check_content_type "/page.htm" "text/html; charset=utf-8"
check_content_type "/style.css" "text/css; charset=utf-8"
check_content_type "/app.js" "text/javascript; charset=utf-8"
check_content_type "/mod.mjs" "text/javascript; charset=utf-8"
check_content_type "/data.json" "application/json"
check_content_type "/notes.txt" "text/plain; charset=utf-8"
check_content_type "/icon.svg" "image/svg+xml"
check_content_type "/feed.xml" "application/xml"
check_content_type "/pic.png" "image/png"
check_content_type "/photo.jpg" "image/jpeg"
check_content_type "/photo2.jpeg" "image/jpeg"
check_content_type "/anim.gif" "image/gif"
check_content_type "/favicon.ico" "image/vnd.microsoft.icon"
check_content_type "/mod.wasm" "application/wasm"
check_content_type "/doc.pdf" "application/pdf"
check_content_type "/README" "application/octet-stream"
check_content_type "/blob.zzz" "application/octet-stream"

# Extensions are matched case-insensitively.
printf 'upper case ext' >"$ROOT/UPPER.TXT"
request h1 "/UPPER.TXT"
check_contains "content-type is case-insensitive" \
    "content-type: text/plain; charset=utf-8" "$REQ_HEADERS"

# ---------------------------------------------------------------------------
# 5. Directory handling.
# ---------------------------------------------------------------------------

log ""
log "== directory handling =="
for combo in "${ALL_COMBOS[@]}"; do
    request "$combo" "/"
    check "$combo GET / serves index.html" "200" "$REQ_STATUS"
    check "$combo GET / body" "<!doctype html><h1>index</h1>" "$(cat "$REQ_BODY_FILE")"
    check_contains "$combo GET / content-type" "content-type: text/html" "$REQ_HEADERS"
done

request h1 "/nested/"
check "GET /nested/ serves nested index" "200" "$REQ_STATUS"
check "GET /nested/ body" "<h1>nested index</h1>" "$(cat "$REQ_BODY_FILE")"

request h1 "/nested"
check "GET /nested (no slash) serves nested index" "200" "$REQ_STATUS"

request h1 "/emptydir/"
check "directory without index.html is 404" "404" "$REQ_STATUS"

request h1 "/nested/deep.txt"
check "nested file status" "200" "$REQ_STATUS"
check "nested file body" "nested body" "$(cat "$REQ_BODY_FILE")"

# ---------------------------------------------------------------------------
# 6. Percent-decoding.
# ---------------------------------------------------------------------------

log ""
log "== percent decoding =="
request h1 "/dir%20with%20space/file%20name.txt"
check "percent-encoded spaces resolve" "200" "$REQ_STATUS"
check "percent-encoded spaces body" "spaced file" "$(cat "$REQ_BODY_FILE")"

request h1 "/notes%2Etxt"
check "percent-encoded dot resolves" "200" "$REQ_STATUS"

request h1 "/notes.txt%GG"
check "invalid percent escape is 400" "400" "$REQ_STATUS"

request h1 "/notes.txt%2"
check "truncated percent escape is 400" "400" "$REQ_STATUS"

# ---------------------------------------------------------------------------
# 7. Path traversal must never escape the document root.
# ---------------------------------------------------------------------------

log ""
log "== path traversal defense =="
traversal_paths=(
    "/../secret.txt"
    "/../../etc/passwd"
    "/nested/../../secret.txt"
    "/%2e%2e/secret.txt"
    "/%2e%2e%2fsecret.txt"
    "/%2E%2E%2F%2E%2E%2Fetc%2Fpasswd"
    "/nested/%2e%2e/%2e%2e/secret.txt"
    "/..%2fsecret.txt"
    "/./../secret.txt"
)

for combo in "${ALL_COMBOS[@]}"; do
    for path in "${traversal_paths[@]}"; do
        request "$combo" "$path"
        if [[ "$REQ_STATUS" == "403" || "$REQ_STATUS" == "404" || "$REQ_STATUS" == "400" ]]; then
            pass "$combo traversal blocked: $path ($REQ_STATUS)"
        else
            fail "$combo traversal blocked: $path" "403/404/400" "$REQ_STATUS"
        fi
        body="$(cat "$REQ_BODY_FILE" 2>/dev/null)"
        if [[ "$body" == *"TOP SECRET"* || "$body" == *"root:"* ]]; then
            fail "$combo traversal leaked content: $path" "no leaked bytes" "$body"
        else
            pass "$combo traversal leaked nothing: $path"
        fi
    done
done

# Absolute-looking request targets are rejected too.
request h1 "//etc/passwd"
if [[ "$REQ_STATUS" == "403" || "$REQ_STATUS" == "404" ]]; then
    pass "absolute-ish path rejected (//etc/passwd => $REQ_STATUS)"
else
    fail "absolute-ish path rejected" "403/404" "$REQ_STATUS"
fi

# A symlink pointing outside the root must not be followed.
request h1 "/escape-link.txt"
if [[ "$REQ_STATUS" == "403" || "$REQ_STATUS" == "404" ]]; then
    pass "symlink escaping root rejected ($REQ_STATUS)"
else
    fail "symlink escaping root rejected" "403/404" "$REQ_STATUS"
fi
if grep -q 'TOP SECRET' "$REQ_BODY_FILE" 2>/dev/null; then
    fail "symlink escape leaks no content" "no leaked bytes" "leaked secret"
else
    pass "symlink escape leaks no content"
fi

# ---------------------------------------------------------------------------
# 8. Missing files and unsupported methods.
# ---------------------------------------------------------------------------

log ""
log "== error responses =="
for combo in "${ALL_COMBOS[@]}"; do
    request "$combo" "/definitely-missing.txt"
    check "$combo missing file is 404" "404" "$REQ_STATUS"
done

for method in POST PUT DELETE PATCH OPTIONS; do
    request h1 "/notes.txt" --request "$method"
    check "$method is 405" "405" "$REQ_STATUS"
    check_contains "$method 405 advertises allow" "allow: get, head" "$REQ_HEADERS"
done

request h2c "/notes.txt" --request POST
check "h2c POST is 405" "405" "$REQ_STATUS"
request tls-h2 "/notes.txt" --request POST
check "tls-h2 POST is 405" "405" "$REQ_STATUS"

# ---------------------------------------------------------------------------
# 9. Empty and large payloads.
# ---------------------------------------------------------------------------

log ""
log "== payload sizes =="
for combo in "${ALL_COMBOS[@]}"; do
    request "$combo" "/empty.txt"
    check "$combo empty file status" "200" "$REQ_STATUS"
    check "$combo empty file downloads 0 bytes" "0" "$REQ_SIZE"
    check_contains "$combo empty file content-length" "content-length: 0" "$REQ_HEADERS"
done

for combo in "${ALL_COMBOS[@]}"; do
    request "$combo" "/large.bin"
    check "$combo large file status" "200" "$REQ_STATUS"
    check "$combo large file size" "$LARGE_SIZE" "$REQ_SIZE"
    check_contains "$combo large file content-length" \
        "content-length: $LARGE_SIZE" "$REQ_HEADERS"
    if cmp -s "$ROOT/large.bin" "$REQ_BODY_FILE"; then
        pass "$combo large file bytes are identical"
    else
        fail "$combo large file bytes are identical" "byte-identical" "differs"
    fi
done

# ---------------------------------------------------------------------------
# 10. Connection reuse and concurrency.
# ---------------------------------------------------------------------------

log ""
log "== reuse and concurrency =="
# Several resources over a single connection per combination.
for combo in "${ALL_COMBOS[@]}"; do
    case "$combo" in
    h1) base="$PLAIN_URL" proto_flags=(--http1.1) ;;
    h2c) base="$PLAIN_URL" proto_flags=(--http2-prior-knowledge) ;;
    tls-h1) base="$TLS_URL" proto_flags=(--http1.1 --insecure) ;;
    tls-h2) base="$TLS_URL" proto_flags=(--http2 --insecure) ;;
    esac
    # curl needs one --output per URL, otherwise later bodies land on stdout.
    args=()
    for resource in /notes.txt /data.json /index.html; do
        args+=(--output /dev/null "${base}${resource}")
    done
    codes="$(curl "${CURL_COMMON[@]}" "${proto_flags[@]}" "${args[@]}" \
        --write-out '%{http_code} ' 2>/dev/null)"
    check "$combo three sequential requests" "200 200 200 " "$codes"
done

# Parallel clients against both listeners. Track the PIDs explicitly: `jobs -p`
# would also report the long-running server job and wait on it forever.
parallel_failures=0
parallel_pids=()
for _ in $(seq 1 12); do
    (
        code="$(curl "${CURL_COMMON[@]}" --http2-prior-knowledge \
            --output /dev/null --write-out '%{http_code}' "$PLAIN_URL/large.bin" 2>/dev/null)"
        [[ "$code" == "200" ]] || exit 1
        code="$(curl "${CURL_COMMON[@]}" --http2 --insecure \
            --output /dev/null --write-out '%{http_code}' "$TLS_URL/large.bin" 2>/dev/null)"
        [[ "$code" == "200" ]] || exit 1
    ) &
    parallel_pids+=($!)
done
for pid in "${parallel_pids[@]}"; do
    wait "$pid" || parallel_failures=$((parallel_failures + 1))
done
check "12 parallel clients across both listeners" "0" "$parallel_failures"

# ---------------------------------------------------------------------------
# 11. The server is still healthy after the whole suite.
# ---------------------------------------------------------------------------

log ""
log "== liveness =="
if kill -0 "$SERVER_PID" 2>/dev/null; then
    pass "server process still running"
else
    fail "server process still running" "alive" "exited"
fi
request h1 "/notes.txt"
check "server still serving after suite" "200" "$REQ_STATUS"

# ---------------------------------------------------------------------------
# Summary.
# ---------------------------------------------------------------------------

log ""
log "-----------------------------------------"
log "passed: $PASS_COUNT   failed: $FAIL_COUNT"
if [[ $FAIL_COUNT -gt 0 ]]; then
    log ""
    log "failing checks:"
    for failure in "${FAILURES[@]}"; do
        log "  - $failure"
    done
    exit 1
fi
log "all checks passed"
exit 0
