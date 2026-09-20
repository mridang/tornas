# Builder runs on the host's native arch (BUILDPLATFORM) and cross-compiles with
# cargo-zigbuild, so arm64 and armv7 images build at native speed without QEMU.
FROM --platform=$BUILDPLATFORM rust:1.95-alpine AS builder
ARG TARGETPLATFORM
RUN apk add --no-cache musl-dev cmake make curl xz clang perl
ENV ZIG_VERSION=0.13.0
RUN ARCH="$(uname -m)" \
 && curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-linux-${ARCH}-${ZIG_VERSION}.tar.xz" | tar -xJ -C /usr/local \
 && mv "/usr/local/zig-linux-${ARCH}-${ZIG_VERSION}" /usr/local/zig
ENV PATH="/usr/local/zig:${PATH}"
RUN cargo install cargo-zigbuild cargo-deb --locked
# linux/amd64 -> x86_64, linux/arm64 -> aarch64, linux/arm/v7 -> armv7
RUN case "$TARGETPLATFORM" in \
      linux/amd64)  echo x86_64-unknown-linux-musl      > /rust-target ;; \
      linux/arm64)  echo aarch64-unknown-linux-musl     > /rust-target ;; \
      linux/arm/v7) echo armv7-unknown-linux-musleabihf > /rust-target ;; \
      *) echo "unsupported TARGETPLATFORM: $TARGETPLATFORM" >&2; exit 1 ;; \
    esac && rustup target add "$(cat /rust-target)"
WORKDIR /app
COPY . .
RUN RUST_TARGET="$(cat /rust-target)" \
 && cargo zigbuild --release --locked --target "$RUST_TARGET" \
 && cp "target/${RUST_TARGET}/release/tornas" /tornas \
 && sha256sum /tornas | sed 's# .*#  tornas#' > /tornas.sha256 \
 && cargo deb --no-build --no-strip --target "$RUST_TARGET" -p tornas -o /tornas.deb \
 && sha256sum /tornas.deb | sed 's# .*#  tornas.deb#' > /tornas.deb.sha256

FROM scratch AS export
COPY --from=builder /tornas /tornas
COPY --from=builder /tornas.sha256 /tornas.sha256
COPY --from=builder /tornas.deb /tornas.deb
COPY --from=builder /tornas.deb.sha256 /tornas.deb.sha256

FROM alpine:3.20 AS runtime
LABEL org.opencontainers.image.source="https://github.com/mridang/tornas"
LABEL org.opencontainers.image.licenses="Apache-2.0"
LABEL org.opencontainers.image.title="tornas"
COPY --from=builder /tornas /usr/local/bin/tornas
ENV TORNAS_DATA_DIR=/data TORNAS_HTTP_LISTEN=0.0.0.0:3030
VOLUME ["/data"]
EXPOSE 3030
HEALTHCHECK --interval=60s --timeout=10s --start-period=120s CMD ["/usr/local/bin/tornas", "health", "--server", "http://127.0.0.1:3030"]
ENTRYPOINT ["/usr/local/bin/tornas"]
CMD ["server"]
