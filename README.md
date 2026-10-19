# tmpfile

Temporary file sharing service with a Rust API and a deploy-ready static web UI.

## Repository layout

```text
.
├── api/                    # Rust/Axum API crate
│   ├── Cargo.toml
│   ├── Cargo.lock
│   ├── Dockerfile
│   ├── contracts/          # Shared API surface fixtures used by both gates
│   └── src/
├── pages/                  # Plain HTML/CSS/JS for Cloudflare Pages
│   ├── index.html
│   ├── config.js           # Public API base URL only; never secrets
│   ├── _headers
│   └── ...icons
├── deploy/nginx/           # Versioned reverse-proxy/CORS template
├── scripts/
│   ├── check-pages.mjs     # Static bundle and API-contract validation
│   ├── check-nginx.sh      # Nginx syntax and live CORS validation
│   └── check-production-cors.sh # Public deployment readiness probe
├── Makefile                # Unified quality/build entry point
├── .env.example
└── .env.production.example
```

The two deployables are intentionally independent:

- `api/` builds the API-only Docker image and preserves the existing HTTP paths.
- `pages/` has no package manager or build output. The directory itself is the static artifact.

## Local development

Start the API with the example development configuration:

```bash
cp .env.example .env
set -a
. ./.env
set +a
make run
```

In another terminal, serve the page directory:

```bash
python3 -m http.server 8788 --directory pages
```

Open `http://127.0.0.1:8788`. The copied UI defaults to
`https://dl.xabcnews.com`. Test the local Rust API directly with curl at
`http://127.0.0.1:8080`. Browser uploads from port 8788 must instead use a
rendered Nginx endpoint because Rust intentionally owns no CORS policy; do not
point the browser straight at port 8080. See
[deploy/nginx/README.md](deploy/nginx/README.md) for the local Origin setup.

Relative `UPLOAD_DIR` values are resolved from `api/` when the server is run via
`make run`. The default local directory is therefore `api/uploads/`.

## Cloudflare Pages

`pages/` is ready to deploy as static HTML. With a Git-integrated Pages project,
use either of these equivalent configurations:

| Setting | Monorepo root | Pages as root |
|---|---|---|
| Root directory | repository root | `pages` |
| Build command | `exit 0` | `exit 0` |
| Build output directory | `pages` | `.` |

An optional build watch include path of `pages/*` prevents API-only commits from
triggering a Pages deployment. Cloudflare documents this no-framework setup in
its [Static HTML guide](https://developers.cloudflare.com/pages/framework-guides/deploy-anything/).

For a direct upload:

```bash
npx wrangler pages deploy pages
```

Before production deployment:

1. Set the page's default Server URL in `pages/config.js` if the API is not
   `https://dl.xabcnews.com`.
2. Set API `PUBLIC_BASE_URL` to the public API/download origin, not to the Pages
   origin.
3. Render the exact production Pages origin into
   `deploy/nginx/tmpfile.conf.template`, validate it, and reload Nginx.
4. Set the GitHub Actions repository variable `TMPFILE_PAGES_ORIGIN` to that
   exact Origin; API deployment is blocked until its public preflight succeeds.
   Leave it unset only when the edge deliberately uses a wildcard policy; the
   preflight check then requires `Access-Control-Allow-Origin: *` with
   `Authorization` listed explicitly in `Access-Control-Allow-Headers`.
5. Never commit `API_KEY` into `pages/`; the UI accepts it at runtime and keeps
   it only in the current tab's `sessionStorage`.

`pages/_headers` adds browser hardening and prevents stale `index.html` caching.
Cloudflare applies this file directly to static responses as described in the
official [`_headers` documentation](https://developers.cloudflare.com/pages/configuration/headers/).

The UI calls the API directly rather than proxying uploads through a Pages
Function. This keeps large-file and streaming behavior under the API/reverse
proxy's control.

## Nginx and CORS policy

Nginx is the single authority for CORS and preflight responses. The versioned
template at `deploy/nginx/tmpfile.conf.template` uses an exact, fail-closed Pages
Origin and permits only `GET`, `HEAD`, `POST`, and `OPTIONS`, with
`Authorization` and `Content-Type` request headers. It also applies headers to
upstream error responses using `always` and emits the required `Vary` fields.

The Rust API contains no CORS middleware or `OPTIONS` shortcut. This avoids
conflicting headers and keeps deployment policy at the public edge. Do not add a
second CORS transform in Cloudflare. CORS is not authentication; configure
`API_KEY` when uploads require authorization.

Rendering, multiple-origin handling, TLS and loopback binding are documented in
[deploy/nginx/README.md](deploy/nginx/README.md). Validate every change with
`make nginx-check` before reloading production Nginx.

## API configuration

All API configuration is provided through environment variables:

| Variable | Default | Description |
|---|---:|---|
| `LISTEN_ADDR` | `0.0.0.0:8080` | API listen address |
| `UPLOAD_DIR` | `./uploads` | File storage directory |
| `MAX_FILE_SIZE` | `268435456` | Maximum file content size in bytes |
| `FILE_EXPIRATION` | `36h` | File lifetime (`s`, `m`, `h`, or `d`) |
| `CLEANUP_INTERVAL` | half the lifetime | Cleanup cadence, clamped to the lifetime |
| `MAX_DOWNLOAD_COUNT` | `0` | General download limit; `0` is unlimited |
| `IMAGE_MAX_DOWNLOAD_COUNT` | `0` | Image-specific limit; `0` is unlimited |
| `PUBLIC_BASE_URL` | request-derived | Public API base used in returned links |
| `API_KEY` | empty | Optional Bearer key for uploads; also enables `/list` |
| `REDIS_ADDR` | empty | Optional Redis endpoint, for example `redis:6379` |
| `REDIS_DB` | `0` | Non-negative Redis database number |
| `RUST_LOG` | `info` | API log filter |

Invalid numeric, duration, or URL settings fail startup instead of
silently weakening limits. Use `.env.production.example` as a deployment
checklist; it intentionally contains no secret values.

## API reference

### `POST /upload`

Upload one `multipart/form-data` field named `file`.

```bash
curl -F 'file=@example.txt' http://127.0.0.1:8080/upload
```

When `API_KEY` is set:

```bash
curl -H 'Authorization: Bearer your-secret-key' \
  -F 'file=@example.txt' \
  http://127.0.0.1:8080/upload
```

The response contains:

```json
{
  "direct_link": "https://dl.example.com/files/0123456789abcdefghijklmno.txt",
  "filename": "example.txt",
  "size": 1024,
  "token": "0123456789abcdefghijklmno",
  "expires_at": "2026-07-15T12:00:00Z",
  "expires_in": "36h"
}
```

Finite limits also add `max_downloads` and `remaining_downloads`.

### `GET|HEAD /files/{token}[.{extension}]`

Downloads the file. `HEAD` returns the metadata headers without a body.

Download responses use `Cache-Control: public, max-age=31536000, immutable`.
Each token URL identifies one immutable file, so browsers and CDNs can reuse the
response without another round trip.

### `POST /video-output/tickets`

Creates a short-lived, one-time video destination without an `Authorization`
header. The returned `upload_url` is suitable for xAI Zero Data Retention
requests: xAI can send the generated bytes directly with a raw `PUT`.

```bash
curl -X POST https://dl.example.com/video-output/tickets
```

The response contains only the URL grok-cloud needs:

```json
{
  "upload_url": "https://dl.example.com/files/0123456789abcdefghijklmno.mp4"
}
```

The ticket itself is the capability; this route and the matching `PUT` route
remain open even when `API_KEY` is configured. A ticket accepts exactly one
non-empty body up to `MAX_FILE_SIZE`:

```bash
curl --upload-file result.mp4 \
  'https://dl.example.com/files/0123456789abcdefghijklmno.mp4'
```

After a successful `PUT`, the same `upload_url` downloads the completed video.
A completed PUT is idempotent: retries return success without replacing the
stored bytes. A concurrent PUT returns `409 Conflict`, and expired tickets
return `410 Gone`. The PUT is server-to-server and does not need browser CORS.
Keep this endpoint scoped to video output—`/list` remains a management endpoint
and ordinary `/upload` still follows `API_KEY`.

### `GET /list`

This management endpoint exists only when `API_KEY` is configured and always
requires the Bearer key.

Query parameters:

- `page`: 1-based page number, default `1`.
- `size`: page size from 1 to 100, default `100`.
- `sort_by`: `uploaded_at`, `size`, or `name`.
- `order`: `asc` or `desc`.

The route paths intentionally remain `/upload`, `/files/*`, and `/list`; moving
the crate into `api/` does not introduce a breaking `/api` URL prefix.

## Quality gates

```bash
make fmt-check
make test
make clippy-ci
make pages-check
make nginx-check
make release-check
```

`make release-check` also runs `cargo audit` and the live Nginx check, so
`cargo-audit`, Nginx, curl, and Python 3 must be installed.
The ignored Redis contract test requires an isolated Redis database:

```bash
cd api
TEST_REDIS_ADDR=127.0.0.1:6379 TEST_REDIS_DB=15 \
  cargo +stable test --locked redis_store_contract -- --ignored
```

GitHub Actions separates API, Pages, and Nginx-only changes. API changes run the
Rust, Redis, local Nginx and security gates before building `api/Dockerfile`, and
the deployment job additionally verifies the public preflight. Nginx-only
changes run their own syntax/behavior workflow without redeploying the API;
Pages changes run the static bundle and API-contract validator.

## Docker

Build from the repository root because `api/Dockerfile` uses the root as its
strictly filtered build context:

```bash
make docker-build
```

Building is local-only. Push explicitly when required:

```bash
make docker-push
```

Run the API image:

```bash
docker run --rm -p 127.0.0.1:8080:8080 \
  -e PUBLIC_BASE_URL=https://dl.xabcnews.com \
  -v tmpfile_uploads:/home/nonroot/uploads \
  proxyxai/tmpfile:rust
```

The image contains only the API. Deploy `pages/` independently to Cloudflare
Pages.

## Operational notes

- Use Redis when metadata must survive API restarts.
- Persist `/home/nonroot/uploads` when running the non-root Docker image.
- Put the API behind HTTPS and set `PUBLIC_BASE_URL` explicitly in production.
- CDN-cache `/files/*` responses. Token URLs identify immutable files and are
  safe to reuse for their cache lifetime.
- Keep the API port private, use the exact Nginx Origin map, and keep API keys
  out of static assets.

## License

Licensed under the [AGPL-3.0](LICENSE). Anyone who offers a modified version as a
network service must make its complete source available to that service's users
under the same license; a commercial license outside the AGPL can be arranged with
the author.

The XABC and xabcnews names and the site icons in `pages/` (`favicon.ico`,
`apple-touch-icon*.png`, `x.svg`, `xabc.svg`) are not covered by the license; forks
should use their own.
