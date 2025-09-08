#!/bin/sh
set -e

export PROXY_CACHE_PATH_CONFIGURATION=${PROXY_CACHE_PATH_CONFIGURATION:-"/dev/shm/nginx"}
export PROXY_CACHE_MAX_SIZE_IN_MB=${PROXY_CACHE_MAX_SIZE_IN_MB:-"1024"}
export PROXY_CACHE_CLEANUP_MAX_DURATION_MS=${PROXY_CACHE_CLEANUP_MAX_DURATION_MS:-"200"}
export PROXY_CACHE_CLEANUP_SLEEP_INTERVAL_MS=${PROXY_CACHE_CLEANUP_SLEEP_INTERVAL_MS:-"50"}
export PROXY_CACHE_CLEANUP_MAX_FILES_DELETED_PER_INTERVAL=${PROXY_CACHE_CLEANUP_MAX_FILES_DELETED_PER_INTERVAL:-"100"}

# HTTPS/mTLS Configuration
export NGINX_HTTPS_PORT=${NGINX_HTTPS_PORT:-"4443"}
export NGINX_SSL_CERT_PATH=${NGINX_SSL_CERT_PATH:-"/etc/ssl/certs/server_full_chain.crt"}
export NGINX_SSL_KEY_PATH=${NGINX_SSL_KEY_PATH:-"/etc/ssl/private/server.key"}
export NGINX_CLIENT_CA_PATH=${NGINX_CLIENT_CA_PATH:-"/etc/ssl/certs/root_ca.crt"}
export NGINX_SSL_VERIFY_CLIENT=${NGINX_SSL_VERIFY_CLIENT:-"optional"}
export NGINX_BACKEND_SCHEME=${NGINX_BACKEND_SCHEME:-"https"}
export NGINX_BACKEND_PORT=${NGINX_BACKEND_PORT:-"8443"}
export NGINX_CLIENT_CERT_PATH=${NGINX_CLIENT_CERT_PATH:-"/etc/ssl/certs/client_full_chain.crt"}
export NGINX_CLIENT_KEY_PATH=${NGINX_CLIENT_KEY_PATH:-"/etc/ssl/private/client.key"}
export NGINX_CLIENT_TRUSTED_CA_PATH=${NGINX_CLIENT_TRUSTED_CA_PATH:-"/etc/ssl/certs/root_ca.crt"}

sed -e "s|{{PROXY_CACHE_PATH_CONFIGURATION}}|$PROXY_CACHE_PATH_CONFIGURATION|g" \
    -e "s|{{PROXY_CACHE_MAX_SIZE_IN_MB}}|$PROXY_CACHE_MAX_SIZE_IN_MB|g" \
    -e "s|{{PROXY_CACHE_CLEANUP_MAX_DURATION_MS}}|$PROXY_CACHE_CLEANUP_MAX_DURATION_MS|g" \
    -e "s|{{PROXY_CACHE_CLEANUP_MAX_FILES_DELETED_PER_INTERVAL}}|$PROXY_CACHE_CLEANUP_MAX_FILES_DELETED_PER_INTERVAL|g" \
    -e "s|{{PROXY_CACHE_CLEANUP_SLEEP_INTERVAL_MS}}|$PROXY_CACHE_CLEANUP_SLEEP_INTERVAL_MS|g" \
    -e "s|{{NGINX_HTTPS_PORT:-4443}}|$NGINX_HTTPS_PORT|g" \
    -e "s|{{NGINX_SSL_CERT_PATH:-/etc/ssl/certs/server_full_chain.crt}}|$NGINX_SSL_CERT_PATH|g" \
    -e "s|{{NGINX_SSL_KEY_PATH:-/etc/ssl/private/server.key}}|$NGINX_SSL_KEY_PATH|g" \
    -e "s|{{NGINX_CLIENT_CA_PATH:-/etc/ssl/certs/root_ca.crt}}|$NGINX_CLIENT_CA_PATH|g" \
    -e "s|{{NGINX_SSL_VERIFY_CLIENT:-optional}}|$NGINX_SSL_VERIFY_CLIENT|g" \
    -e "s|{{NGINX_BACKEND_SCHEME:-https}}|$NGINX_BACKEND_SCHEME|g" \
    -e "s|{{NGINX_BACKEND_PORT:-8443}}|$NGINX_BACKEND_PORT|g" \
    -e "s|{{NGINX_CLIENT_CERT_PATH:-/etc/ssl/certs/client_full_chain.crt}}|$NGINX_CLIENT_CERT_PATH|g" \
    -e "s|{{NGINX_CLIENT_KEY_PATH:-/etc/ssl/private/client.key}}|$NGINX_CLIENT_KEY_PATH|g" \
    -e "s|{{NGINX_CLIENT_TRUSTED_CA_PATH:-/etc/ssl/certs/root_ca.crt}}|$NGINX_CLIENT_TRUSTED_CA_PATH|g" \
    /nginx.conf.template > /etc/nginx/nginx.conf

# Start Nginx in the background, redirecting output to /dev/null
nginx > /dev/null 2>&1 &

# Execute the main application
exec statsig_forward_proxy "$@"
