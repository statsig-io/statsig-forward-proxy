# TLS and mTLS Setup

## Overview

Statsig Forward Proxy uses nginx as a reverse proxy to provide HTTP and HTTPS endpoints with TLS/mTLS support.

## Architecture

```
Client → Nginx (8000/8443) → SFP (localhost:8080)
```

## Modes

### HTTP Only (Default)
```bash
docker run statsig/forward-proxy:latest grpc-and-http redis
```
- Port 8000: HTTP

### HTTP + HTTPS
```bash
docker run \
  -v /path/to/certs:/etc/tls:ro \
  statsig/forward-proxy:latest \
  grpc-and-http redis \
  --x509-server-cert-path /etc/tls/server.crt \
  --x509-server-key-path /etc/tls/server.key \
  --x509-client-cert-path /etc/tls/ca.crt
```
- Port 8000: HTTP 
- Port 8443: HTTPS/mTLS

### HTTPS Only
```bash
docker run \
  -v /path/to/certs:/etc/tls:ro \
  statsig/forward-proxy:latest \
  grpc-and-http redis \
  --x509-server-cert-path /etc/tls/server.crt \
  --x509-server-key-path /etc/tls/server.key \
  --x509-client-cert-path /etc/tls/ca.crt \
  --enforce-tls
```
- Port 8443: HTTPS/mTLS only

## Certificate Requirements

- **Server Certificate**: TLS certificate for the server
- **Server Key**: Private key for the server certificate
- **CA Certificate**: Certificate Authority for client verification (mTLS)

## SSL Verification Options

- `--ssl-verify-client on`: Require client certificates (strict mTLS)
- `--ssl-verify-client optional`: Accept both with/without client certificates (default)
- `--ssl-verify-client off`: Disable client certificate verification (TLS only)

## Security

- SFP HTTP server listens on `127.0.0.1:8080` (localhost only)
- External clients cannot bypass nginx to access SFP directly
- All TLS termination and mTLS verification handled by nginx 