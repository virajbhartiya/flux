# syntax=docker/dockerfile:1
FROM rust:1.95-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && mkdir -p /out \
 && cp target/release/flux target/release/flux-server target/release/flux-import-git /out/

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates git tini \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --home /var/lib/flux --create-home flux
COPY --from=build /out/ /usr/local/bin/
USER flux
WORKDIR /var/lib/flux
ENV FLUX_LISTEN=0.0.0.0:7878 FLUX_DATA_DIR=/var/lib/flux/data
EXPOSE 7878
VOLUME ["/var/lib/flux"]
HEALTHCHECK --interval=10s --timeout=3s CMD ["/bin/bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/7878 && printf 'GET /-/readyz HTTP/1.0\\r\\n\\r\\n' >&3 && grep -q ready <&3"]
ENTRYPOINT ["/usr/bin/tini", "--", "flux-server"]
