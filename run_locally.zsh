#!/bin/zsh

# Because we need to fork the process to run gcloud ssh. Creating a trap to kill subprocesses.
trap "trap - SIGTERM && kill -- -$$" SIGINT SIGTERM EXIT
export REDIS_TLS=false
export REDIS_ENTERPRISE_HOST="localhost"
export REDIS_ENTERPRISE_PORT=6379
export STATSIG_ENDPOINT="http://0.0.0.0:8000"
# export STATSD_SOCKET="/tmp/dsd.socket"
# export DOGSTATSD_MAX_BUFFER_SIZE="1800"

echo "Did you configure redis in the script?... If not, this might look like its hanging..."
cargo run --bin server grpc-and-http redis --debug-logging --datadog-logging
