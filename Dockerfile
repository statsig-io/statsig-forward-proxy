FROM rust:1.94.0-alpine3.22 AS builder

RUN apk upgrade --no-cache && apk add --no-cache git curl build-base autoconf automake libtool pkgconfig libressl-dev musl-dev gcc libc-dev g++ libffi-dev

# Install protoc
ARG TARGETPLATFORM
ARG GRPC_HEALTH_PROBE_VERSION=v0.4.45
RUN \
  case ${TARGETPLATFORM} in \
    "linux/amd64") PROTO_ARCH="x86_64" ;; \
    "linux/arm64") PROTO_ARCH="aarch_64" ;; \
    *) echo "Unsupported architecture: ${TARGETPLATFORM}" >&2; exit 1 ;; \
  esac && \
  curl -LO https://github.com/protocolbuffers/protobuf/releases/download/v26.0/protoc-26.0-linux-${PROTO_ARCH}.zip && \
  unzip protoc-26.0-linux-${PROTO_ARCH}.zip && \
  cp ./bin/protoc /usr/local/bin/protoc && \
  rm protoc-26.0-linux-${PROTO_ARCH}.zip

# Download grpc-health-probe binary
RUN \
  case ${TARGETPLATFORM} in \
    "linux/amd64") GRPC_ARCH="amd64"; GRPC_SHA256="b926a8a1513dad75a2a2196973e9f754d241e325593bbdf90ad1c24faa86e263" ;; \
    "linux/arm64") GRPC_ARCH="arm64"; GRPC_SHA256="7a4a922e8f6bd22f96564071e5b951ca63234d10474be9705e985802e47422d7" ;; \
    *) echo "Unsupported architecture: ${TARGETPLATFORM}" >&2; exit 1 ;; \
  esac && \
  curl -L -o grpc-health-probe \
  https://github.com/grpc-ecosystem/grpc-health-probe/releases/download/${GRPC_HEALTH_PROBE_VERSION}/grpc_health_probe-linux-${GRPC_ARCH} && \
  echo "${GRPC_SHA256}  grpc-health-probe" | sha256sum -c - && \
  chmod +x grpc-health-probe 

# create a new empty shell project, copy dependencies
# and install to allow caching of dependencies
RUN USER=root cargo new --bin statsig_forward_proxy
WORKDIR /statsig_forward_proxy
COPY ./.cargo ./.cargo
COPY ./Cargo.lock ./Cargo.lock
COPY ./Cargo.toml ./Cargo.toml
COPY ./rust-toolchain.toml ./rust-toolchain.toml
COPY ./benches ./benches
RUN cp ./src/main.rs ./src/server.rs
RUN cp ./src/main.rs ./src/client.rs
RUN rustup update
RUN cargo build --release
RUN rm src/*.rs

# Copy Important stuff and then build final binary
COPY ./src ./src
COPY ./build.rs ./build.rs
COPY ./api-interface-definitions ./api-interface-definitions
RUN rm ./target/release/deps/server*
RUN cargo build --release

FROM nginxinc/nginx-unprivileged:1.29.5-alpine3.23

USER root
RUN apk add --no-cache --upgrade 'zlib>=1.3.2-r0'
USER 101

# Copy the build artifact from the build stage
COPY --from=builder /statsig_forward_proxy/target/release/server /usr/local/bin/statsig_forward_proxy

# Copy grpc-health-probe binary
COPY --from=builder /grpc-health-probe /usr/local/bin/grpc-health-probe

# Copy other necessary files
COPY ./.cargo /app/.cargo
COPY ./Rocket.toml /app/Rocket.toml

# Set working directory
WORKDIR /app

# Set environment variable
ENV ROCKET_ENV=prod

EXPOSE 8000 8443

COPY nginx-http-only.conf.template /nginx-http-only.conf.template
COPY nginx-http-https.conf.template /nginx-http-https.conf.template
COPY nginx-https-only.conf.template /nginx-https-only.conf.template

# Create an entrypoint script (set executable at copy time)
COPY --chmod=0755 entrypoint.sh /entrypoint.sh

# Use ENTRYPOINT to run the script as non-root
ENTRYPOINT ["/entrypoint.sh"]
