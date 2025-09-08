# TLS and mTLS Setup

# Table of Contents
1. [Overview](#overview)
2. [Architecture](#architecture)
3. [Certificate Requirements](#certificate-requirements)
4. [Configuration Options](#configuration-options)
5. [Docker Deployment](#docker-deployment)
6. [Local Development](#local-development)
7. [Testing](#testing)
8. [Troubleshooting](#troubleshooting)

## Overview

The Statsig Forward Proxy supports both TLS (Transport Layer Security) and mTLS (Mutual TLS) for secure communication. This document covers how to enable and configure these security features.

**Key Features:**
- TLS encryption for client-server communication
- mTLS for mutual authentication between clients and servers
- Nginx proxy with TLS/mTLS termination and forwarding
- Direct SFP connections with TLS/mTLS support
- Flexible certificate management

## Architecture

The proxy supports two deployment patterns:

### Pattern 1: Client → Nginx → SFP (Recommended)
```
┌────────┐   TLS/mTLS   ┌────────┐    mTLS    ┌────────┐
│ Client │ ────────→ │ Nginx  │ ────────→ │  SFP   │
└────────┘   (4443)     └────────┘   (8443)   └────────┘
```

### Pattern 2: Client → SFP (Direct)
```
┌────────┐   TLS/mTLS   ┌────────┐
│ Client │ ────────→ │  SFP   │
└────────┘   (8443)     └────────┘
```

## Certificate Requirements

You need the following certificates for full TLS/mTLS setup:

### Required Files
- **Root CA Certificate** (`root_ca.crt`) - Trust anchor for all certificates
- **Server Certificate + Key** (`server_full_chain.crt`, `server.key`) - For SFP HTTPS server
- **Client Certificate + Key** (`client_full_chain.crt`, `client.key`) - For mTLS authentication

### Certificate Chain Structure
```
Root CA
└── Intermediate CA (optional)
    ├── Server Certificate (for SFP)
    └── Client Certificate (for clients/Nginx)
```

### Generating Test Certificates

For development, use the provided script:

```bash
cd x509_test_certs
./gen_certs.sh
```

This generates:
- `root/certs/root_ca.crt` - Root CA
- `intermediate/certs/intermediate_ca.crt` - Intermediate CA
- `intermediate/certs/server_full_chain.crt` - Server certificate with full chain
- `intermediate/private/server.key` - Server private key
- `intermediate/certs/client_full_chain.crt` - Client certificate with full chain
- `intermediate/private/client.key` - Client private key

## Configuration Options

### Environment Variables

The following environment variables control TLS/mTLS behavior:

#### Nginx Configuration
- `NGINX_HTTPS_PORT` - HTTPS port (default: 4443)
- `NGINX_SSL_CERT_PATH` - Server certificate path (default: `/etc/ssl/certs/server_full_chain.crt`)
- `NGINX_SSL_KEY_PATH` - Server private key path (default: `/etc/ssl/private/server.key`)
- `NGINX_CLIENT_CA_PATH` - Client CA certificate for mTLS (default: `/etc/ssl/certs/root_ca.crt`)
- `NGINX_SSL_VERIFY_CLIENT` - Client verification mode: `on`, `optional`, `off` (default: `optional`)

#### Nginx → SFP mTLS
- `NGINX_CLIENT_CERT_PATH` - Nginx client certificate (default: `/etc/ssl/certs/client_full_chain.crt`)
- `NGINX_CLIENT_KEY_PATH` - Nginx client private key (default: `/etc/ssl/private/client.key`)
- `NGINX_CLIENT_TRUSTED_CA_PATH` - CA to verify SFP server (default: `/etc/ssl/certs/root_ca.crt`)
- `NGINX_BACKEND_SCHEME` - Backend protocol: `http`, `https` (default: `https`)
- `NGINX_BACKEND_PORT` - SFP port (default: 8443)

### SFP Command Line Arguments

Enable TLS/mTLS on the SFP server:

```bash
statsig-forward-proxy grpc-and-http redis \
  --x509-server-cert-path /path/to/server_full_chain.crt \
  --x509-server-key-path /path/to/server.key \
  --x509-client-cert-path /path/to/root_ca.crt \
  --enforce-tls true \
  --enforce-mtls
```

**Arguments:**
- `--x509-server-cert-path` - Path to server certificate
- `--x509-server-key-path` - Path to server private key  
- `--x509-client-cert-path` - Path to CA certificate for client verification
- `--enforce-tls true` - Require TLS connections
- `--enforce-mtls` - Require client certificates (flag, no value)

## Docker Deployment

### Basic TLS Setup

```bash
docker run --rm \
  -p 4443:4443 \
  -v /path/to/certs/server_full_chain.crt:/etc/ssl/certs/server_full_chain.crt:ro \
  -v /path/to/certs/server.key:/etc/ssl/private/server.key:ro \
  -v /path/to/certs/root_ca.crt:/etc/ssl/certs/root_ca.crt:ro \
  statsig/forward-proxy:latest \
  grpc-and-http redis \
  --x509-server-cert-path /etc/ssl/certs/server_full_chain.crt \
  --x509-server-key-path /etc/ssl/private/server.key \
  --enforce-tls true
```

### Full mTLS Setup

```bash
docker run --rm \
  -p 4443:4443 \
  -p 8443:8443 \
  -v /path/to/certs/server_full_chain.crt:/etc/ssl/certs/server_full_chain.crt:ro \
  -v /path/to/certs/server.key:/etc/ssl/private/server.key:ro \
  -v /path/to/certs/root_ca.crt:/etc/ssl/certs/root_ca.crt:ro \
  -v /path/to/certs/client_full_chain.crt:/etc/ssl/certs/client_full_chain.crt:ro \
  -v /path/to/certs/client.key:/etc/ssl/private/client.key:ro \
  statsig/forward-proxy:latest \
  grpc-and-http redis \
  --x509-server-cert-path /etc/ssl/certs/server_full_chain.crt \
  --x509-server-key-path /etc/ssl/private/server.key \
  --x509-client-cert-path /etc/ssl/certs/root_ca.crt \
  --enforce-tls true \
  --enforce-mtls
```

### Environment Variable Override

```bash
docker run --rm \
  -p 4443:4443 \
  -e NGINX_SSL_VERIFY_CLIENT=on \
  -e NGINX_HTTPS_PORT=443 \
  -v /path/to/certs:/etc/ssl:ro \
  statsig/forward-proxy:latest \
  grpc-and-http redis \
  --enforce-tls true \
  --enforce-mtls
```

## Local Development

### Using run_locally.zsh

For local development with TLS/mTLS, modify `run_locally.zsh`:

```bash
#!/bin/zsh
# ... existing exports ...

cargo run --bin server grpc-and-http redis --debug-logging \
  --x509-server-cert-path "./x509_test_certs/intermediate/certs/server_full_chain.crt" \
  --x509-server-key-path "./x509_test_certs/intermediate/private/server.key" \
  --x509-client-cert-path "./x509_test_certs/root/certs/root_ca.crt" \
  --enforce-tls true \
  --enforce-mtls
```

### Local Nginx Setup

```bash
# Set environment variables
export NGINX_CLIENT_CA_PATH="./x509_test_certs/root/certs/root_ca.crt"
export NGINX_SSL_CERT_PATH="./x509_test_certs/intermediate/certs/server_full_chain.crt"
export NGINX_SSL_KEY_PATH="./x509_test_certs/intermediate/private/server.key"

# Render nginx config
sed -e "s|{{NGINX_CLIENT_CA_PATH:-/etc/ssl/certs/root_ca.crt}}|$NGINX_CLIENT_CA_PATH|g" \
    -e "s|{{NGINX_SSL_CERT_PATH:-/etc/ssl/certs/server_full_chain.crt}}|$NGINX_SSL_CERT_PATH|g" \
    -e "s|{{NGINX_SSL_KEY_PATH:-/etc/ssl/private/server.key}}|$NGINX_SSL_KEY_PATH|g" \
    nginx.conf.template > nginx.conf

# Start nginx
nginx -p "$PWD" -c nginx.conf
```

## Testing

### TLS-Only (No Client Certificate)

Test Nginx TLS endpoint:
```bash
curl -v --cacert ./x509_test_certs/root/certs/root_ca.crt \
  https://localhost:4443/v1/download_config_specs/your-key.json
```

Test SFP TLS endpoint directly:
```bash
curl -v --cacert ./x509_test_certs/root/certs/root_ca.crt \
  https://localhost:8443/v1/download_config_specs/your-key.json
```

### mTLS (With Client Certificate)

Test with client certificate:
```bash
curl -v \
  --cacert ./x509_test_certs/root/certs/root_ca.crt \
  --cert ./x509_test_certs/intermediate/certs/client_full_chain.crt \
  --key ./x509_test_certs/intermediate/private/client.key \
  https://localhost:4443/v1/download_config_specs/your-key.json
```

### Skip Certificate Verification (Development Only)

```bash
curl -vk https://localhost:4443/v1/download_config_specs/your-key.json
```

## Troubleshooting

### Common Issues

#### "SSL certificate problem: self signed certificate in certificate chain"
**Cause:** Client doesn't trust your CA certificate.
**Solution:** Use `--cacert` with your root CA or `-k` to skip verification.

#### "nginx: [emerg] unknown ssl_client_s_dn_cn variable"
**Cause:** Nginx version doesn't support certain SSL variables.
**Solution:** Remove unsupported SSL variables from nginx config.

#### "borrow of moved value: config.port"
**Cause:** Rust code tries to use config after it's been moved.
**Solution:** Capture values before moving config to Rocket.

#### Client certificate verification fails with root CA
**Cause:** Client sends only leaf certificate, server needs intermediate CA to build chain.
**Solutions:**
1. Use client certificate with full chain (`client_full_chain.crt`)
2. Configure server with CA bundle containing root + intermediate CAs
3. Use intermediate CA as trust anchor instead of root CA

### Debug Steps

1. **Verify certificate chain:**
   ```bash
   openssl verify -CAfile root/certs/root_ca.crt \
     -untrusted intermediate/certs/intermediate_ca.crt \
     intermediate/certs/server.crt
   ```

2. **Check certificate details:**
   ```bash
   openssl x509 -in server.crt -text -noout
   ```

3. **Test TLS handshake:**
   ```bash
   openssl s_client -connect localhost:4443 -CAfile root_ca.crt
   ```

4. **Check Nginx configuration:**
   ```bash
   nginx -t -c /path/to/nginx.conf
   ```

### Port Reference

| Service | Protocol | Port | Purpose |
|---------|----------|------|---------|
| Nginx   | HTTP     | 8000 | Plain HTTP proxy |
| Nginx   | HTTPS    | 4443 | TLS/mTLS proxy |
| SFP     | HTTP     | 8001 | Plain HTTP server |
| SFP     | HTTPS    | 8443 | TLS/mTLS server |

For production deployments, consider:
- Using port 443 for HTTPS (map 4443:443)
- Disabling HTTP ports for security
- Using proper CA-signed certificates
- Implementing certificate rotation 