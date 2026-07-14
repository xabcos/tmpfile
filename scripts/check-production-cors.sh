#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 2 ]]; then
    echo "usage: $0 <public-upload-url> <exact-pages-origin>" >&2
    exit 2
fi

upload_url=$1
allowed_origin=$2

if [[ "$upload_url" != https://* && \
    !( "${TMPFILE_CORS_CHECK_ALLOW_HTTP:-}" == 1 && \
       "$upload_url" == http://127.0.0.1:* ) ]]; then
    echo "production upload URL must use https" >&2
    exit 2
fi

origin_host=${allowed_origin#https://}
if [[ "$origin_host" == "$allowed_origin" || -z "$origin_host" || \
    "$origin_host" == */* || "$origin_host" == *\?* || \
    "$origin_host" == *\#* || "$origin_host" == *@* ]]; then
    echo "TMPFILE_PAGES_ORIGIN must be one exact https Origin without a path" >&2
    exit 2
fi

for command in curl grep sed tr; do
    if ! command -v "$command" >/dev/null 2>&1; then
        echo "production CORS check requires '$command'" >&2
        exit 1
    fi
done

tmp_dir=$(mktemp -d)
cleanup() {
    rm -rf -- "$tmp_dir"
}
trap cleanup EXIT

headers="$tmp_dir/headers"
status=$(curl \
    --silent \
    --show-error \
    --max-time 20 \
    --request OPTIONS \
    --header "Origin: $allowed_origin" \
    --header 'Access-Control-Request-Method: POST' \
    --header 'Access-Control-Request-Headers: authorization,content-type' \
    --dump-header "$headers" \
    --output /dev/null \
    --write-out '%{http_code}' \
    "$upload_url")
tr -d '\r' <"$headers" >"$tmp_dir/headers.clean"
headers="$tmp_dir/headers.clean"

if [[ "$status" != 204 ]]; then
    echo "production nginx preflight returned HTTP $status instead of 204" >&2
    sed -n '1,120p' "$headers" >&2
    exit 1
fi

assert_header() {
    local name=$1
    local expected_value=$2
    local header_count
    header_count=$(grep -Eic "^${name}:" "$headers" || true)
    if [[ "$header_count" -ne 1 ]] || \
        ! grep -Fxiq "$name: $expected_value" "$headers"; then
        echo "production nginx must return exactly: $name: $expected_value" >&2
        sed -n '1,120p' "$headers" >&2
        exit 1
    fi
}

assert_header 'Access-Control-Allow-Origin' "$allowed_origin"
assert_header 'Access-Control-Allow-Methods' 'GET, HEAD, POST, OPTIONS'
assert_header 'Access-Control-Allow-Headers' 'Authorization, Content-Type'

denied_origin='https://cors-denied.invalid'
denied_headers="$tmp_dir/denied-headers"
denied_status=$(curl \
    --silent \
    --show-error \
    --max-time 20 \
    --request OPTIONS \
    --header "Origin: $denied_origin" \
    --header 'Access-Control-Request-Method: POST' \
    --dump-header "$denied_headers" \
    --output /dev/null \
    --write-out '%{http_code}' \
    "$upload_url")
tr -d '\r' <"$denied_headers" >"$tmp_dir/denied-headers.clean"

if [[ "$denied_status" != 204 ]]; then
    echo "production nginx denied-origin preflight returned HTTP $denied_status instead of 204" >&2
    sed -n '1,120p' "$tmp_dir/denied-headers.clean" >&2
    exit 1
fi
if grep -Eiq '^Access-Control-Allow-Origin:' "$tmp_dir/denied-headers.clean"; then
    echo "production nginx exposed CORS to denied Origin $denied_origin" >&2
    sed -n '1,120p' "$tmp_dir/denied-headers.clean" >&2
    exit 1
fi

echo "Production nginx CORS preflight check passed for $allowed_origin."
