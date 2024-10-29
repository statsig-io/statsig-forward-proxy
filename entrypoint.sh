#!/bin/sh
set -e

# Start Nginx in the background, redirecting output to /dev/null
nginx -g 'daemon off; error_log /dev/stderr error;' > /dev/null 2>&1 &

# Execute the main application
exec statsig_forward_proxy "$@"
