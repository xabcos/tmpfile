# tmpfile

Temporary file sharing service with automatic expiration and Redis persistence support.

## Features

- ⏰ Automatic file expiration (configurable)
- 📊 Download count limiting
- 🔐 Optional API key authentication
- 💾 Redis persistence support
- 🚀 High-performance Rust implementation
- 🐳 Docker ready with docker-compose support

## Quick Start

### Local Development

```bash
cargo run
```

### Docker

```bash
# Build and run
docker build -t tmpfile .
docker run -d --network host tmpfile

# Or use docker-compose (includes Redis)
docker-compose up -d
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
  "direct_link": "http://localhost:8080/files/abc123xyz0123456.txt",
  "filename": "example.txt",
  "size": 1024,
  "token": "abc123xyz0123456",
  "expires_at": "2025-10-10T18:00:00Z",
  "expires_in": "36h"
}
```

### Download a File

```bash
curl -O http://localhost:8080/files/abc123xyz0123456.txt
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
| `MAX_DOWNLOAD_COUNT` | `0` | Max downloads per file (0 = unlimited) |
| `API_KEY` | - | Optional API key for authentication |
| `REDIS_ADDR` | - | Redis server address (e.g., `redis:6379`) |
| `REDIS_DB` | `0` | Redis database number |
| `RUST_LOG` | `info` | Log level (`trace`, `debug`, `info`, `warn`, `error`) |

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
cargo build --release
```

### Run Tests

```bash
cargo test
```

### Format Code

```bash
cargo fmt
```

### Lint

```bash
cargo clippy
```

## Architecture

- **Storage Backend**: Supports both in-memory and Redis persistence
- **File Distribution**: Uses subdirectory sharding for better filesystem performance
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
  "size": number,
  "token": "string",
  "expires_at": "ISO8601 datetime",
  "expires_in": "string",
  "max_downloads": number (optional),
  "remaining_downloads": number (optional)
}
```

### GET /files/{token}

Download a file by token.

**Response:**
- File content with appropriate headers

## Performance

- **Memory Usage**: ~30MB base + active connections
- **Request Handling**: Async I/O with Tokio runtime
- **Concurrency**: Thread-safe with lock-free operations where possible

## Security

✅ **Best Practices:**
- Always use `API_KEY` in production
- Deploy behind HTTPS reverse proxy (Nginx/Traefik)
- Set appropriate `MAX_FILE_SIZE` limits
- Use Redis for persistence across restarts
- Monitor disk usage of upload volumes
- Set reasonable `FILE_EXPIRATION` for auto-cleanup

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

### Redis Connection Failed

Check Redis is accessible:
```bash
docker exec tmpfile ping -c 1 redis
```

## License

MIT License - See LICENSE file for details

## Contributing

Contributions welcome! Please open an issue or PR.
