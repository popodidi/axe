# axe server image: `axe server` with the dashboard embedded (embed-ui).
# Built and published by .github/workflows/release.yml on a `v*` tag; the
# release binary tarball is copied out of this image, so both are one build.

FROM node:24-bookworm-slim AS web
WORKDIR /src/web
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY web/ ./
RUN npm run build

FROM rust:1.90-bookworm AS build
ARG CARGO_BUILD_JOBS=1
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS} CARGO_TERM_COLOR=never
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY --from=web /src/web/out ./web/out
RUN cargo build --release --locked -p axe-os --features embed-ui

FROM debian:bookworm-slim
# python3 serves maintenance pods that reuse this image (database copies).
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates python3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/axe /usr/local/bin/axe
ARG GIT_COMMIT
RUN test -n "$GIT_COMMIT" && echo "$GIT_COMMIT" > /etc/axe-revision
USER 10000:10000
ENV AXE_DB=/data/axe.db AXE_CONFIG=/data/config.toml AXE_TOKEN_FILE=/auth/token
EXPOSE 8790
ENTRYPOINT ["axe"]
CMD ["server", "--listen", "0.0.0.0:8790"]
