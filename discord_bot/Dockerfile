# TatarBot Discord adapter.
# Build: docker build -t tatarbot-discord discord_bot

FROM rust:1.99-slim-trixie AS build
# libopus (voice) is linked statically from Debian's libopus.a, so the distroless
# runtime image needs no extra libraries. pkg-config would link system libraries
# dynamically, so libopus_sys is pointed at a copy of the archive instead
# (arch-independent path for amd64 and arm64 builds; the build script adds /lib).
RUN apt-get update \
    && apt-get install -y --no-install-recommends libopus-dev \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /opt/opus/lib \
    && cp /usr/lib/*-linux-gnu/libopus.a /opt/opus/lib/
ENV LIBOPUS_NO_PKG=1 LIBOPUS_LIB_DIR=/opt/opus LIBOPUS_STATIC=1
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
# Default radio stations, embedded in the binary.
COPY stations.toml ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/tatarbot-discord /tatarbot-discord

# glibc + CA certificates, no shell, runs as uid 65532.
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /tatarbot-discord /usr/local/bin/tatarbot-discord
# Plain log lines for docker logs / CloudWatch (tracing-subscriber honours NO_COLOR).
ENV NO_COLOR=1
ENTRYPOINT ["/usr/local/bin/tatarbot-discord"]
