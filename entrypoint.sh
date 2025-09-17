#!/bin/sh
set -e

export PROXY_CACHE_PATH_CONFIGURATION=${PROXY_CACHE_PATH_CONFIGURATION:-"/dev/shm/nginx"}
export PROXY_CACHE_MAX_SIZE_IN_MB=${PROXY_CACHE_MAX_SIZE_IN_MB:-"1024"}
export PROXY_CACHE_CLEANUP_MAX_DURATION_MS=${PROXY_CACHE_CLEANUP_MAX_DURATION_MS:-"200"}
export PROXY_CACHE_CLEANUP_SLEEP_INTERVAL_MS=${PROXY_CACHE_CLEANUP_SLEEP_INTERVAL_MS:-"50"}
export PROXY_CACHE_CLEANUP_MAX_FILES_DELETED_PER_INTERVAL=${PROXY_CACHE_CLEANUP_MAX_FILES_DELETED_PER_INTERVAL:-"100"}

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

sed -e "s|{{PROXY_CACHE_PATH_CONFIGURATION}}|$PROXY_CACHE_PATH_CONFIGURATION|g" \
    -e "s|{{PROXY_CACHE_MAX_SIZE_IN_MB}}|$PROXY_CACHE_MAX_SIZE_IN_MB|g" \
    -e "s|{{PROXY_CACHE_CLEANUP_MAX_DURATION_MS}}|$PROXY_CACHE_CLEANUP_MAX_DURATION_MS|g" \
    -e "s|{{PROXY_CACHE_CLEANUP_MAX_FILES_DELETED_PER_INTERVAL}}|$PROXY_CACHE_CLEANUP_MAX_FILES_DELETED_PER_INTERVAL|g" \
    -e "s|{{PROXY_CACHE_CLEANUP_SLEEP_INTERVAL_MS}}|$PROXY_CACHE_CLEANUP_SLEEP_INTERVAL_MS|g" \
    -e "s|{{SSL_CERT_PATH}}|$SSL_CERT_PATH|g" \
    -e "s|{{SSL_KEY_PATH}}|$SSL_KEY_PATH|g" \
    -e "s|{{CLIENT_CA_PATH}}|$CLIENT_CA_PATH|g" \
    -e "s|{{SSL_VERIFY_CLIENT}}|$SSL_VERIFY_CLIENT|g" \
    "$TEMPLATE_FILE" > /etc/nginx/nginx.conf

if ! nginx -t >/dev/null 2>&1; then
    nginx -t
    exit 1
fi

nginx > /dev/null 2>&1 &

exec statsig_forward_proxy "$@"
