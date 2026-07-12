# tmpfile

Temporary file sharing service with automatic expiration and Redis persistence support.

## Features

- ⏰ Automatic file expiration (configurable)
- 📊 Download count limiting
- 🔐 Optional API key authentication
- 💾 Redis persistence support
- 🚀 High-performance Rust implementation
- 🐳 Docker-ready, non-root runtime image

## Quick Start

### Local Development

```bash
rustup update stable
cargo +stable run --locked
```

### Docker

```bash
# Build and run
docker build --pull -t tmpfile .
docker run -d --network host tmpfile
```

**Note**: If you encounter `Timed out proxy starting the userland proxy` error, use `--network host` mode:
```bash
docker run -d --network host tmpfile
```

## Usage Examples

### Upload a File

```bash
curl -F "file=@example.txt" http://localhost:8080/upload
```

**Response:**
```json
{
  "direct_link": "http://localhost:8080/files/0123456789abcdefghijklmno.txt",
  "filename": "example.txt",
  "size": 1024,
  "token": "0123456789abcdefghijklmno",
  "expires_at": "2025-10-10T18:00:00Z",
  "expires_in": "36h"
}
```

### Download a File

```bash
curl -O http://localhost:8080/files/0123456789abcdefghijklmno.txt
# Or visit directly in browser
```

### Upload with API Key

```bash
curl -H "Authorization: Bearer your-secret-key" \
     -F "file=@example.txt" \
     http://localhost:8080/upload
```

## Configuration

All configuration is done via environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `LISTEN_ADDR` | `0.0.0.0:8080` | Server listen address |
| `UPLOAD_DIR` | `./uploads` | Upload directory path |
| `MAX_FILE_SIZE` | `268435456` | Max file size in bytes (256MB) |
| `FILE_EXPIRATION` | `36h` | File expiration time (supports: `s`, `m`, `h`, `d`) |
| `CLEANUP_INTERVAL` | half of expiration | Cleanup cadence, clamped to the file lifetime |
| `MAX_DOWNLOAD_COUNT` | `0` | Max accepted download attempts per file (0 = unlimited) |
| `IMAGE_MAX_DOWNLOAD_COUNT` | `0` | Max accepted download attempts for image files (0 = unlimited) |
| `PUBLIC_BASE_URL` | - | Public base URL used in returned links, e.g. `https://dl.example.com` |
| `API_KEY` | - | Optional API key for authentication |
| `REDIS_ADDR` | - | Redis server address (e.g., `redis:6379`) |
| `REDIS_DB` | `0` | Redis database number |
| `RUST_LOG` | `info` | Log level (`trace`, `debug`, `info`, `warn`, `error`) |

Invalid, negative, zero, or out-of-range numeric settings fail startup with a clear error instead of silently weakening limits.
An accepted download consumes one count even if the client later interrupts the transfer.

### Production Example

```yaml
services:
  tmpfile:
    image: tmpfile:latest
    ports:
      - "9000:8080"
    environment:
      - API_KEY=your-secret-key
      - REDIS_ADDR=redis:6379
      - FILE_EXPIRATION=1h
      - MAX_DOWNLOAD_COUNT=3
      - MAX_FILE_SIZE=268435456
    volumes:
      - uploads_data:/home/nonroot/uploads
    networks:
      - network

  redis:
    image: redis:7-alpine
    command: redis-server --appendonly yes
    volumes:
      - redis_data:/data
    networks:
      - network

volumes:
  uploads_data:
  redis_data:

networks:
  network:
    driver: bridge
```

## Development

### Build

```bash
cargo +stable build --locked --release
```

### Run Tests

```bash
cargo +stable test --locked --all-targets --all-features
```

The Redis contract test is intentionally isolated from the default suite. Run it against a disposable Redis instance or database:

```bash
TEST_REDIS_ADDR=127.0.0.1:6379 TEST_REDIS_DB=15 \
  cargo +stable test --locked redis_store_contract -- --ignored
```

### Format Code

```bash
cargo +stable fmt --all
```

### Lint

```bash
cargo +stable clippy --locked --all-targets --all-features
```

The complete local release gate is available through `make release-check` (and requires `cargo-audit`).

## Architecture

- **Storage Backend**: Supports both in-memory and Redis persistence
- **File Distribution**: Uses subdirectory sharding for better filesystem performance
- **Upload Commit**: Writes to hidden partial files, then atomically links completed data without overwriting an existing token
- **Cleanup Worker**: Automatic background cleanup of expired files
- **Logging**: Structured logging with `tracing`

## API Reference

### POST /upload

Upload a file.

**Headers:**
- `Authorization: Bearer <api_key>` (if API_KEY is set)

**Body:**
- `multipart/form-data` with field name `file`

**Response:**
```json
{
  "direct_link": "string",
  "filename": "string",
  "size": 1024,
  "token": "string",
  "expires_at": "ISO8601 datetime",
  "expires_in": "string",
  "max_downloads": 3,
  "remaining_downloads": 3
}
```

`max_downloads` and `remaining_downloads` are omitted when downloads are unlimited.

### GET /files/{token}

Download a file by token.

**Response:**
- File content with appropriate headers

### GET /list

List active files. This management endpoint is disabled unless `API_KEY` is configured, and then requires `Authorization: Bearer <api_key>`.

Query parameters:

- `page`: 1-based page number (default `1`)
- `size`: page size from 1 to 100 (default `100`)
- `sort_by`: `uploaded_at`, `size`, or `name`
- `order`: `asc` or `desc`

## Performance

- **Request Handling**: Uploads and downloads use asynchronous streaming I/O
- **I/O Buffers**: File reads and writes use 128 KiB buffers
- **Metadata Concurrency**: The in-memory backend is split across 64 `RwLock` shards; Redis mutations use atomic pipelines/Lua where required

## Security

✅ **Best Practices:**
- Always use `API_KEY` in production
- Deploy behind HTTPS reverse proxy (Nginx/Traefik)
- Set appropriate `MAX_FILE_SIZE` limits
- Use Redis for persistence across restarts
- Monitor disk usage of upload volumes
- Set reasonable `FILE_EXPIRATION` for auto-cleanup
- Set `PUBLIC_BASE_URL` behind a reverse proxy so returned links do not depend on client-controlled `Host` headers

## Troubleshooting

### Docker Port Mapping Timeout

If you encounter: `driver failed programming external connectivity on endpoint tmpfile: Timed out proxy starting the userland proxy`

**Solutions:**
1. Use host network mode: `docker run --network host tmpfile`
2. Restart Docker daemon: `systemctl restart docker`
3. Use different port: `docker run -p 9000:8080 tmpfile`

### Permission Denied (uploads directory)

The application runs as non-root user. Ensure volumes have correct permissions:
```bash
docker run --rm -v tmpfile_uploads_data:/vol busybox chown -R 65532:65532 /vol
```
