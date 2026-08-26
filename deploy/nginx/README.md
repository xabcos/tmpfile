# tmpfile nginx edge

Nginx is the only owner of browser CORS and `OPTIONS` handling. The Rust API
does not emit `Access-Control-Allow-*` headers and intentionally returns its
normal method error when clients bypass Nginx with a preflight request.

## Render and install

`tmpfile.conf.template` is an `http`-context template. It contains a fail-closed
`__TMPFILE_PAGES_ORIGIN__` token because the real Pages domain is deployment
specific and must not be guessed.

Render one exact production origin:

```bash
PAGES_ORIGIN=https://your-project.pages.dev
sed "s|__TMPFILE_PAGES_ORIGIN__|${PAGES_ORIGIN}|g" \
  deploy/nginx/tmpfile.conf.template \
  | sudo tee /etc/nginx/conf.d/tmpfile.conf >/dev/null
```

If the main `nginx.conf` does not already load `conf.d`, add the following line
inside its `http { ... }` block:

```nginx
include /etc/nginx/conf.d/*.conf;
```

Then validate and reload atomically:

```bash
sudo nginx -t
sudo nginx -s reload
```

This install/reload is intentionally a host operation; the Docker deployment
does not overwrite the system Nginx configuration. Install it before deploying
the Rust image that removes application-level CORS. Then set the GitHub Actions
repository variable `TMPFILE_PAGES_ORIGIN` to the same exact Origin. The API
deployment job performs a public preflight check and stops before SSH deployment
if the edge policy is missing or different.

For a custom domain plus a `pages.dev` domain, add a second exact row to the
rendered `map`. Do not replace the map with `*`. Preview deployment hostnames
must also be listed explicitly if they need API access.

The checked-in server listens on port 80 for an existing TLS/Cloudflare edge and
uses `server_name dl.xabclabs.com`. Add the site's certificate/listen directives
when Nginx itself terminates public TLS.

For local browser testing, render `http://127.0.0.1:8788` as the allowed Origin,
adapt the rendered listener/server name to an unused local endpoint, and enter
that Nginx endpoint in the page's **Server URL** field. Port 8080 remains a
curl/API-only endpoint.

## Runtime boundary

- The host upstream is `127.0.0.1:8080`; change it only when Nginx and the API
  do not share the host network.
- A containerized API must listen on `0.0.0.0:8080` internally but publish only
  `127.0.0.1:8080` on the host.
- Production must set `PUBLIC_BASE_URL=https://dl.xabclabs.com`; Nginx also
  overwrites `Host` and `X-Forwarded-Proto`.
- The 257 MiB Nginx body limit leaves framing room above the API's 256 MiB file
  limit. Keep both limits coordinated if either changes.
- `POST /video-output/tickets` and the write-once `PUT /files/{token}.mp4`
  deliberately require no Authorization header so xAI's ZDR worker can upload
  directly. Treat the returned URL as a capability and keep ticket response
  bodies out of logs.
- Nginx strips any upstream `Access-Control-*` response policy before adding
  the checked-in one. Do not add another CORS transform at Cloudflare or in the
  Rust service.

## Verification

```bash
make nginx-check
```

The check renders an isolated test origin, runs `nginx -t`, starts Nginx on a
temporary loopback port, and verifies allowed/disallowed preflights plus CORS
headers on proxied success and error responses. The dummy upstream also verifies
that `Authorization` reaches the API. The check never edits the system Nginx
configuration.

Run the same read-only production readiness check used by CI with:

```bash
./scripts/check-production-cors.sh \
  https://dl.xabclabs.com/upload \
  https://your-project.pages.dev
```
