#!/bin/sh
set -e

export PROXY_CACHE_PATH_CONFIGURATION=${PROXY_CACHE_PATH_CONFIGURATION:-"/dev/shm/nginx"}
export PROXY_CACHE_DOWNLOAD_PATH_CONFIGURATION=${PROXY_CACHE_DOWNLOAD_PATH_CONFIGURATION:-"${PROXY_CACHE_PATH_CONFIGURATION}/download_cache"}
export PROXY_CACHE_DOWNLOAD_ID_LIST_PATH_CONFIGURATION=${PROXY_CACHE_DOWNLOAD_ID_LIST_PATH_CONFIGURATION:-"${PROXY_CACHE_PATH_CONFIGURATION}/download_id_list"}
export PROXY_CACHE_MAX_SIZE_IN_MB=${PROXY_CACHE_MAX_SIZE_IN_MB:-"1024"}
export PROXY_CACHE_TTL=${PROXY_CACHE_TTL:-"5s"}
export PROXY_CACHE_CLEANUP_MAX_DURATION_MS=${PROXY_CACHE_CLEANUP_MAX_DURATION_MS:-"200"}
export PROXY_CACHE_CLEANUP_SLEEP_INTERVAL_MS=${PROXY_CACHE_CLEANUP_SLEEP_INTERVAL_MS:-"50"}
export PROXY_CACHE_CLEANUP_MAX_FILES_DELETED_PER_INTERVAL=${PROXY_CACHE_CLEANUP_MAX_FILES_DELETED_PER_INTERVAL:-"100"}
export NGINX_WORKER_PROCESSES=${NGINX_WORKER_PROCESSES:-"auto"}

HTTPS_PORT="8443"
ENFORCE_TLS="false"
SSL_VERIFY_CLIENT="optional"
SSL_CERT_PATH=""
SSL_KEY_PATH=""
CLIENT_CA_PATH=""

validate_file_path() {
    local path="$1"
    local arg_name="$2"
    
    if [ -z "$path" ]; then
        echo "Error: $arg_name requires a value" >&2
        exit 1
    fi
    
    if [ "$path" = "--"* ]; then
        echo "Error: $arg_name value cannot start with --" >&2
        exit 1
    fi
}

validate_boolean() {
    local value="$1"
    local arg_name="$2"
    
    if [ -z "$value" ]; then
        echo "Error: $arg_name requires a value" >&2
        exit 1
    fi
    
    if [ "$value" != "true" ] && [ "$value" != "false" ]; then
        echo "Error: $arg_name must be 'true' or 'false', got: $value" >&2
        exit 1
    fi
}

validate_nginx_duration() {
    local value="$1"
    local arg_name="$2"

    if [ -z "$value" ]; then
        echo "Error: $arg_name requires a value" >&2
        exit 1
    fi

    # Keep this strict to avoid generating invalid nginx.conf and to keep sed substitution safe.
    # nginx supports time units; we accept a common subset customers will use.
    case "$value" in
        ''|*[!0-9a-zA-Z]*)
            echo "Error: $arg_name must be an nginx time like '5s' or '500ms', got: $value" >&2
            exit 1
            ;;
        *[0-9]ms|*[0-9]s|*[0-9]m|*[0-9]h|*[0-9]d)
            ;;
        *)
            echo "Error: $arg_name must include a unit (e.g. '500ms', '5s', '1m'); got: $value" >&2
            exit 1
            ;;
    esac
}

validate_nginx_worker_processes() {
    local value="$1"
    local arg_name="$2"

    if [ -z "$value" ]; then
        echo "Error: $arg_name requires a value" >&2
        exit 1
    fi

    case "$value" in
        auto)
            ;;
        *[!0-9]*)
            echo "Error: $arg_name must be 'auto' or a positive integer, got: $value" >&2
            exit 1
            ;;
        0)
            echo "Error: $arg_name must be greater than 0 when numeric, got: $value" >&2
            exit 1
            ;;
        *)
            ;;
    esac
}

NEXT_ARG_IS=""
for arg in "$@"; do
    case "$NEXT_ARG_IS" in
        "cert")
            validate_file_path "$arg" "--x509-server-cert-path"
            SSL_CERT_PATH="$arg"
            NEXT_ARG_IS=""
            ;;
        "key")
            validate_file_path "$arg" "--x509-server-key-path"
            SSL_KEY_PATH="$arg"
            NEXT_ARG_IS=""
            ;;
        "client-ca")
            validate_file_path "$arg" "--x509-client-cert-path"
            CLIENT_CA_PATH="$arg"
            NEXT_ARG_IS=""
            ;;
        "ssl-verify-client")
            if [ "$arg" != "on" ] && [ "$arg" != "optional" ] && [ "$arg" != "off" ]; then
                echo "Error: --ssl-verify-client must be 'on', 'optional', or 'off'" >&2
                exit 1
            fi
            SSL_VERIFY_CLIENT="$arg"
            NEXT_ARG_IS=""
            ;;

        *)
            case "$arg" in
                --x509-server-cert-path)
                    NEXT_ARG_IS="cert"
                    ;;
                --x509-server-key-path)
                    NEXT_ARG_IS="key"
                    ;;
                --x509-client-cert-path)
                    NEXT_ARG_IS="client-ca"
                    ;;
                --enforce-tls)
                    ENFORCE_TLS="true"
                    ;;
                --ssl-verify-client)
                    NEXT_ARG_IS="ssl-verify-client"
                    ;;
            esac
            ;;
    esac
done

validate_nginx_duration "$PROXY_CACHE_TTL" "PROXY_CACHE_TTL"
validate_nginx_worker_processes "$NGINX_WORKER_PROCESSES" "NGINX_WORKER_PROCESSES"

case "$NEXT_ARG_IS" in
    "cert")
        echo "Error: --x509-server-cert-path requires a value" >&2
        exit 1
        ;;
    "key")
        echo "Error: --x509-server-key-path requires a value" >&2
        exit 1
        ;;
    "client-ca")
        echo "Error: --x509-client-cert-path requires a value" >&2
        exit 1
        ;;
    "ssl-verify-client")
        echo "Error: --ssl-verify-client requires a value (on/optional/off)" >&2
        exit 1
        ;;
esac

if [ -z "$SSL_CERT_PATH" ] || [ -z "$SSL_KEY_PATH" ] || [ -z "$CLIENT_CA_PATH" ]; then
    TEMPLATE_FILE="/nginx-http-only.conf.template"
elif [ "$ENFORCE_TLS" = "true" ]; then
    TEMPLATE_FILE="/nginx-https-only.conf.template"
else
    TEMPLATE_FILE="/nginx-http-https.conf.template"
fi

# Ensure the cache directories exist under the configured writable cache root.
mkdir -p "$PROXY_CACHE_PATH_CONFIGURATION" "$PROXY_CACHE_DOWNLOAD_PATH_CONFIGURATION" "$PROXY_CACHE_DOWNLOAD_ID_LIST_PATH_CONFIGURATION"

NGINX_CONF="/tmp/nginx.conf"

sed -e "s|{{PROXY_CACHE_PATH_CONFIGURATION}}|$PROXY_CACHE_PATH_CONFIGURATION|g" \
    -e "s|{{PROXY_CACHE_DOWNLOAD_PATH_CONFIGURATION}}|$PROXY_CACHE_DOWNLOAD_PATH_CONFIGURATION|g" \
    -e "s|{{PROXY_CACHE_DOWNLOAD_ID_LIST_PATH_CONFIGURATION}}|$PROXY_CACHE_DOWNLOAD_ID_LIST_PATH_CONFIGURATION|g" \
    -e "s|{{PROXY_CACHE_MAX_SIZE_IN_MB}}|$PROXY_CACHE_MAX_SIZE_IN_MB|g" \
    -e "s|{{PROXY_CACHE_TTL}}|$PROXY_CACHE_TTL|g" \
    -e "s|{{NGINX_WORKER_PROCESSES}}|$NGINX_WORKER_PROCESSES|g" \
    -e "s|{{PROXY_CACHE_CLEANUP_MAX_DURATION_MS}}|$PROXY_CACHE_CLEANUP_MAX_DURATION_MS|g" \
    -e "s|{{PROXY_CACHE_CLEANUP_MAX_FILES_DELETED_PER_INTERVAL}}|$PROXY_CACHE_CLEANUP_MAX_FILES_DELETED_PER_INTERVAL|g" \
    -e "s|{{PROXY_CACHE_CLEANUP_SLEEP_INTERVAL_MS}}|$PROXY_CACHE_CLEANUP_SLEEP_INTERVAL_MS|g" \
    -e "s|{{SSL_CERT_PATH}}|$SSL_CERT_PATH|g" \
    -e "s|{{SSL_KEY_PATH}}|$SSL_KEY_PATH|g" \
    -e "s|{{CLIENT_CA_PATH}}|$CLIENT_CA_PATH|g" \
    -e "s|{{SSL_VERIFY_CLIENT}}|$SSL_VERIFY_CLIENT|g" \
    "$TEMPLATE_FILE" > "$NGINX_CONF"

if ! nginx -t -g 'pid /tmp/nginx.pid;' -c "$NGINX_CONF" >/dev/null 2>&1; then
    nginx -t -g 'pid /tmp/nginx.pid;' -c "$NGINX_CONF"
    exit 1
fi

nginx -g 'pid /tmp/nginx.pid;' -c "$NGINX_CONF" > /dev/null 2>&1 &

exec statsig_forward_proxy "$@"
