FROM amd64/rust:1.75-alpine3.19 as builder

RUN apk update && apk add git curl build-base autoconf automake libtool pkgconfig libressl-dev musl-dev gcc libc-dev g++ libffi-dev

RUN curl -LO https://github.com/protocolbuffers/protobuf/releases/download/v26.0/protoc-26.0-linux-x86_64.zip
RUN unzip protoc-26.0-linux-x86_64.zip
RUN cp ./bin/protoc /usr/bin/protoc

# create a new empty shell project, copy dependencies
# and install to allow caching of dependencies
RUN USER=root cargo new --bin statsig_forward_proxy
WORKDIR /statsig_forward_proxy
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
COPY ./protos ./protos
RUN rm ./target/release/deps/server*
RUN cargo build --release

FROM amd64/rust:1.75-slim

# copy the build artifact from the build stage
WORKDIR /app
COPY ./.cargo ./.cargo
COPY ./Rocket.toml ./Rocket.toml
COPY --from=builder /statsig_forward_proxy/target/release/server /usr/local/bin/statsig_forward_proxy

ENTRYPOINT [ "statsig_forward_proxy" ]
