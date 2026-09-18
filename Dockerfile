# syntax=docker/dockerfile:1

# The server only: built without the player, so no ALSA headers, no audio device
# and no terminal graphics. protoc is vendored by the build script, so the builder
# needs nothing beyond the Rust image.
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto proto
COPY src src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --no-default-features \
    && cp target/release/tuneterm /tuneterm

# glibc and nothing else; runs as an unprivileged user unless compose says which.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /tuneterm /usr/local/bin/tuneterm
EXPOSE 7700
VOLUME ["/music"]
ENTRYPOINT ["/usr/local/bin/tuneterm"]
CMD ["serve", "/music"]
