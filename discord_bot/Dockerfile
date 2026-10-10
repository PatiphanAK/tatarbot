# TatarBot Discord adapter.
# Build: docker build -t tatarbot-discord discord_bot

# ---- ffmpeg/ffprobe for library uploads: a minimal, static, audio-only build.
# Only the file protocol, the accepted input formats and their audio decoders, and
# the libopus encoder + Ogg muxer are compiled in: no network protocols, no HLS/m3u,
# concat or image/video demuxers, so a crafted upload can't make ffmpeg open
# anything but the local file (the bot also passes protocol/format whitelists).
# Source signature checked against the FFmpeg release key (B4322F04D67658D8) when
# the hash below was pinned.
FROM debian:trixie-slim AS ffmpeg
ARG FFMPEG_VERSION=9.0.2
ARG FFMPEG_SHA256=8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential ca-certificates curl libopus-dev pkg-config xz-utils \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
RUN curl -fsSLo ffmpeg.tar.xz "https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.xz" \
    && echo "${FFMPEG_SHA256}  ffmpeg.tar.xz" | sha256sum -c - \
    && tar -xf ffmpeg.tar.xz --strip-components=1 \
    && rm ffmpeg.tar.xz
RUN ./configure --prefix=/opt/ffmpeg \
        --disable-everything --disable-autodetect --disable-network \
        --disable-doc --disable-debug --disable-ffplay \
        --disable-avdevice --disable-swscale --disable-x86asm \
        --enable-static --disable-shared --pkg-config-flags=--static \
        --extra-ldflags=-static \
        --enable-libopus --enable-swresample \
        --enable-protocol=file \
        --enable-demuxer=mp3,ogg,flac,wav,mov,matroska,aac \
        --enable-decoder=mp3,mp3float,aac,aac_fixed,alac,flac,vorbis,opus,pcm_s16le,pcm_s16be,pcm_s24le,pcm_s32le,pcm_f32le,pcm_f64le,pcm_u8,pcm_alaw,pcm_mulaw \
        --enable-parser=mpegaudio,aac,flac,vorbis,opus \
        --enable-encoder=libopus --enable-muxer=ogg \
        --enable-filter=abuffer,abuffersink,aformat,anull,aresample,atrim \
    && make -j"$(nproc)" \
    && make install \
    && /opt/ffmpeg/bin/ffmpeg -hide_banner -protocols | grep -qx '  file' \
    && ! /opt/ffmpeg/bin/ffmpeg -hide_banner -demuxers 2>/dev/null | grep -qE ' (hls|concat|image2|http) ' \
    && ! ldd /opt/ffmpeg/bin/ffmpeg >/dev/null 2>&1

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
# sqlx compiles queries from the committed cache (.cargo/config.toml: SQLX_OFFLINE);
# migrations are embedded in the binary.
COPY .cargo ./.cargo
COPY .sqlx ./.sqlx
COPY migrations ./migrations
# Default radio stations, embedded in the binary.
COPY stations.toml ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/tatarbot-discord /tatarbot-discord

# glibc + CA certificates, no shell, runs as uid 65532.
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=ffmpeg /opt/ffmpeg/bin/ffmpeg /opt/ffmpeg/bin/ffprobe /usr/local/bin/
# Supabase's public root CA, for DATABASE_URL=...?sslmode=verify-full&sslrootcert=<this>.
COPY certs/supabase-root-2021.crt /etc/tatarbot/supabase-root-2021.crt
COPY --from=build /tatarbot-discord /usr/local/bin/tatarbot-discord
# Plain log lines for docker logs / CloudWatch (tracing-subscriber honours NO_COLOR).
ENV NO_COLOR=1
ENTRYPOINT ["/usr/local/bin/tatarbot-discord"]
