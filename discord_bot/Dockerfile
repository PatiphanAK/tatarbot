# TatarBot Discord adapter.
# Build: docker build -t tatarbot-discord discord_bot

FROM rust:1.99-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
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
