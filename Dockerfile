FROM --platform=$BUILDPLATFORM rust:1.83-alpine3.19 AS builder

RUN apk update && apk add git curl build-base autoconf automake libtool pkgconfig libressl-dev musl-dev gcc libc-dev g++ libffi-dev

# Install protoc
ARG TARGETPLATFORM
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
    "linux/amd64") GRPC_ARCH="amd64" ;; \
    "linux/arm64") GRPC_ARCH="arm64" ;; \
    *) echo "Unsupported architecture: ${TARGETPLATFORM}" >&2; exit 1 ;; \
  esac && \
  curl -L -o grpc-health-probe \
  https://github.com/grpc-ecosystem/grpc-health-probe/releases/download/v0.4.37/grpc_health_probe-linux-${GRPC_ARCH} && \
  chmod +x grpc-health-probe 

# create a new empty shell project, copy dependencies
# and install to allow caching of dependencies
RUN USER=root cargo new --bin statsig_forward_proxy
WORKDIR /statsig_forward_proxy
COPY ./.cargo ./.cargo
COPY ./Cargo.lock ./Cargo.lock
COPY ./Cargo.toml ./Cargo.toml
COPY ./rust-toolchain.toml ./rust-toolchain.toml
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

FROM nginx:alpine

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

# Expose port 8001 for Nginx and 8000 for the proxy
EXPOSE 8000 8001

# Copy Nginx configuration
COPY nginx.conf.template /nginx.conf.template
# Create an entrypoint script
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

# Use ENTRYPOINT to run the script
ENTRYPOINT ["/entrypoint.sh"]
