# Static musl build: the binary also runs inside tnt-mapproxy's Alpine image
# (COPY --from=<this image> /usr/local/bin/rmapproxy …).
FROM rust:1.98-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
# Dependencies in their own layer, so source changes don't rebuild them.
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release --locked && rm -rf src
COPY src src
RUN touch src/main.rs && cargo build --release --locked && strip target/release/rmapproxy

FROM alpine:3.22
RUN apk upgrade --no-cache && adduser -D -u 150 mapproxy
COPY --from=build /src/target/release/rmapproxy /usr/local/bin/rmapproxy
# Installed under mapproxy-seed's name too: tnt-mapproxy's admin app and seed jobs run that
# command and recognise seed processes by it.
RUN ln -s /usr/local/bin/rmapproxy /usr/local/bin/mapproxy-seed
USER mapproxy
ENTRYPOINT ["/usr/local/bin/rmapproxy"]
