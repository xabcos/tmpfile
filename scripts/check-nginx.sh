#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
template="$repo_root/deploy/nginx/tmpfile.conf.template"
allowed_origin="https://pages.example.test"
denied_origin="https://pages.example.test.evil"
nginx_bin=${NGINX_BIN:-nginx}

for command in "$nginx_bin" curl python3 sed grep; do
    if ! command -v "$command" >/dev/null 2>&1; then
        echo "nginx check requires '$command'" >&2
        exit 1
    fi
done

if ! grep -Fq '__TMPFILE_PAGES_ORIGIN__' "$template"; then
    echo "nginx template must retain the explicit Pages origin placeholder" >&2
    exit 1
fi

tmp_dir=$(mktemp -d)
nginx_pid=""
backend_pid=""
cleanup() {
    if [[ -n "$nginx_pid" ]] && kill -0 "$nginx_pid" 2>/dev/null; then
        kill -TERM "$nginx_pid" 2>/dev/null || true
        wait "$nginx_pid" 2>/dev/null || true
    fi
    if [[ -n "$backend_pid" ]] && kill -0 "$backend_pid" 2>/dev/null; then
        kill -TERM "$backend_pid" 2>/dev/null || true
        wait "$backend_pid" 2>/dev/null || true
    fi
    rm -rf -- "$tmp_dir"
}
trap cleanup EXIT

read -r listen_port backend_port < <(python3 - <<'PY'
import socket

sockets = []
for _ in range(2):
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    sockets.append(sock)
ports = [sock.getsockname()[1] for sock in sockets]
for sock in sockets:
    sock.close()
print(*ports)
PY
)

rendered="$tmp_dir/tmpfile.conf"
sed \
    -e "s|__TMPFILE_PAGES_ORIGIN__|$allowed_origin|g" \
    -e "s|listen 80;|listen 127.0.0.1:$listen_port;|" \
    -e '/listen \[::\]:80;/d' \
    -e "s|server 127.0.0.1:8080;|server 127.0.0.1:$backend_port;|" \
    "$template" >"$rendered"

if grep -Fq '__TMPFILE_' "$rendered"; then
    echo "nginx test render contains an unresolved placeholder" >&2
    exit 1
fi

mkdir -p \
    "$tmp_dir/client_temp" \
    "$tmp_dir/proxy_temp" \
    "$tmp_dir/fastcgi_temp" \
    "$tmp_dir/uwsgi_temp" \
    "$tmp_dir/scgi_temp"

main_config="$tmp_dir/nginx.conf"
{
    printf 'worker_processes 1;\n'
    printf 'error_log "%s" notice;\n' "$tmp_dir/error.log"
    printf 'pid "%s";\n' "$tmp_dir/nginx.pid"
    printf 'events { worker_connections 64; }\n'
    printf 'http {\n'
    printf '  access_log off;\n'
    printf '  client_body_temp_path "%s";\n' "$tmp_dir/client_temp"
    printf '  proxy_temp_path "%s";\n' "$tmp_dir/proxy_temp"
    printf '  fastcgi_temp_path "%s";\n' "$tmp_dir/fastcgi_temp"
    printf '  uwsgi_temp_path "%s";\n' "$tmp_dir/uwsgi_temp"
    printf '  scgi_temp_path "%s";\n' "$tmp_dir/scgi_temp"
    printf '  include "%s";\n' "$rendered"
    printf '}\n'
} >"$main_config"

"$nginx_bin" -t -p "$tmp_dir/" -c "$main_config"

python3 - "$backend_port" >"$tmp_dir/backend.log" 2>&1 <<'PY' &
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Direct preflight fixtures for the production checker's wildcard mode:
# (Allow-Origin, Allow-Methods, Allow-Headers). Nginx answers OPTIONS itself,
# so these are only reachable on the backend port.
WILDCARD_CASES = {
    "/wildcard/star-authorization": ("*", "GET,POST,PUT,DELETE,OPTIONS,PATCH", "*, Authorization"),
    "/wildcard/listed": ("*", "GET, POST", "Content-Type, Authorization"),
    "/wildcard/star": ("*", "GET,POST,PUT,DELETE,OPTIONS,PATCH", "*"),
    "/wildcard/x-authorization": ("*", "GET, POST", "X-Authorization, Content-Type"),
    "/wildcard/x-star": ("*", "GET, POST", "Content-Type, X-*"),
    "/wildcard/x-post": ("*", "GET, X-POST", "*, Authorization"),
    "/wildcard/exact-origin": ("https://pages.example.test", "GET, POST", "*"),
}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def respond(self, status, body):
        self.send_response(status)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        # The edge must strip any upstream CORS policy before applying its own.
        self.send_header("Access-Control-Allow-Origin", "*")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/error":
            self.respond(500, b"upstream error")
        else:
            self.respond(200, b"ok")

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(length)
        if self.headers.get("Authorization") != "Bearer nginx-check":
            self.respond(401, b"missing authorization")
        else:
            self.respond(200, b"uploaded")

    def do_OPTIONS(self):
        origin, methods, headers = WILDCARD_CASES[self.path]
        self.send_response(204)
        self.send_header("Access-Control-Allow-Origin", origin)
        self.send_header("Access-Control-Allow-Methods", methods)
        self.send_header("Access-Control-Allow-Headers", headers)
        self.end_headers()

    def log_message(self, _format, *_args):
        pass


ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
PY
backend_pid=$!

"$nginx_bin" -p "$tmp_dir/" -c "$main_config" -g 'daemon off;' \
    >"$tmp_dir/stdout.log" 2>&1 &
nginx_pid=$!

base_url="http://127.0.0.1:$listen_port"
url="$base_url/upload"
ready=false
for _ in {1..50}; do
    if curl --fail --silent --output /dev/null "$url"; then
        ready=true
        break
    fi
    if ! kill -0 "$nginx_pid" 2>/dev/null; then
        break
    fi
    sleep 0.1
done
if [[ "$ready" != true ]]; then
    echo "nginx test instance failed to start" >&2
    sed -n '1,160p' "$tmp_dir/stdout.log" >&2 || true
    sed -n '1,160p' "$tmp_dir/error.log" >&2 || true
    sed -n '1,160p' "$tmp_dir/backend.log" >&2 || true
    exit 1
fi

request_headers() {
    curl --silent --show-error --dump-header - --output /dev/null "$@" | tr -d '\r'
}

assert_status() {
    local response=$1
    local expected_status=$2
    local status_line=${response%%$'\n'*}
    if ! grep -Eq "^HTTP/[0-9.]+ ${expected_status}([[:space:]]|$)" <<<"$status_line"; then
        echo "unexpected nginx status (wanted $expected_status): $status_line" >&2
        printf '%s\n' "$response" >&2
        exit 1
    fi
}

assert_header() {
    local response=$1
    local name=$2
    local expected_value=$3
    local header_count
    header_count=$(grep -Eic "^${name}:" <<<"$response" || true)
    if [[ "$header_count" -ne 1 ]] || \
        ! grep -Fxiq "$name: $expected_value" <<<"$response"; then
        echo "expected exactly one nginx header: $name: $expected_value" >&2
        printf '%s\n' "$response" >&2
        exit 1
    fi
}

assert_header_absent() {
    local response=$1
    local name=$2
    if grep -Eiq "^${name}:" <<<"$response"; then
        echo "unexpected nginx response header: $name" >&2
        printf '%s\n' "$response" >&2
        exit 1
    fi
}

allowed_preflight=$(request_headers \
    --request OPTIONS \
    --header "Origin: $allowed_origin" \
    --header 'Access-Control-Request-Method: POST' \
    --header 'Access-Control-Request-Headers: authorization,content-type' \
    "$url")
assert_status "$allowed_preflight" 204
assert_header "$allowed_preflight" 'Access-Control-Allow-Origin' "$allowed_origin"
assert_header "$allowed_preflight" 'Access-Control-Allow-Methods' 'GET, HEAD, POST, OPTIONS'
assert_header "$allowed_preflight" 'Access-Control-Allow-Headers' 'Authorization, Content-Type'
assert_header "$allowed_preflight" 'Vary' 'Origin, Access-Control-Request-Method, Access-Control-Request-Headers'

denied_preflight=$(request_headers \
    --request OPTIONS \
    --header "Origin: $denied_origin" \
    --header 'Access-Control-Request-Method: POST' \
    "$url")
assert_status "$denied_preflight" 204
assert_header_absent "$denied_preflight" 'Access-Control-Allow-Origin'

allowed_upload=$(request_headers \
    --request POST \
    --header "Origin: $allowed_origin" \
    --header 'Authorization: Bearer nginx-check' \
    --header 'Content-Type: application/octet-stream' \
    --data-binary 'nginx-check-body' \
    "$url")
assert_status "$allowed_upload" 200
assert_header "$allowed_upload" 'Access-Control-Allow-Origin' "$allowed_origin"

denied_actual=$(request_headers --header "Origin: $denied_origin" "$base_url/health")
assert_status "$denied_actual" 200
assert_header_absent "$denied_actual" 'Access-Control-Allow-Origin'

# `always` must retain CORS on upstream errors so browser clients can read them.
upstream_error=$(request_headers --header "Origin: $allowed_origin" "$base_url/error")
assert_status "$upstream_error" 500
assert_header "$upstream_error" 'Access-Control-Allow-Origin' "$allowed_origin"

# Exercise the same public-readiness checker used by the deployment workflow.
TMPFILE_CORS_CHECK_ALLOW_HTTP=1 \
    "$repo_root/scripts/check-production-cors.sh" "$url" "$allowed_origin"

# Without an exact origin the checker demands a wildcard policy, so this
# exact-origin template must fail it.
if TMPFILE_CORS_CHECK_ALLOW_HTTP=1 \
    "$repo_root/scripts/check-production-cors.sh" "$url" "" >/dev/null 2>&1; then
    echo "wildcard production check accepted an exact-origin policy" >&2
    exit 1
fi

# The wildcard mode must compare whole header tokens.
backend_url="http://127.0.0.1:$backend_port/wildcard"
for case in star-authorization listed; do
    TMPFILE_CORS_CHECK_ALLOW_HTTP=1 \
        "$repo_root/scripts/check-production-cors.sh" "$backend_url/$case" "" >/dev/null
done
for case in star x-authorization x-star x-post exact-origin; do
    if TMPFILE_CORS_CHECK_ALLOW_HTTP=1 \
        "$repo_root/scripts/check-production-cors.sh" "$backend_url/$case" "" >/dev/null 2>&1; then
        echo "wildcard production check accepted the $case policy" >&2
        exit 1
    fi
done

echo "Nginx syntax and CORS behavior checks passed."
