#!/bin/sh
set -e

export PROXY_CACHE_PATH_CONFIGURATION=${PROXY_CACHE_PATH_CONFIGURATION:-"/dev/shm/nginx"}
export PROXY_CACHE_MAX_SIZE_IN_MB=${PROXY_CACHE_MAX_SIZE_IN_MB:-"1024"}

sed -e "s|{{PROXY_CACHE_PATH_CONFIGURATION}}|$PROXY_CACHE_PATH_CONFIGURATION|g" \
    -e "s|{{PROXY_CACHE_MAX_SIZE_IN_MB}}|$PROXY_CACHE_MAX_SIZE_IN_MB|g" \
    /nginx.conf.template > /etc/nginx/nginx.conf

# Start Nginx in the background, redirecting output to /dev/null
nginx > /dev/null 2>&1 &

# Execute the main application
exec statsig_forward_proxy "$@"
