# The homeostat appliance image: the supervisor binary plus the runtime a
# deployed house needs — git for the repo surface, uv and a pre-installed
# Python for the units. Run it with the house repo mounted at /house:
#
#   docker run -v /path/to/house:/house -p 7447:7447 ghcr.io/freol35241/homeostat
#
# The builder stage always runs on the build host's architecture and
# cross-compiles toward $TARGETARCH, so a multi-arch `docker buildx build`
# never runs rustc under QEMU emulation.

FROM --platform=$BUILDPLATFORM rust:1-bookworm AS build
ARG TARGETARCH
WORKDIR /src

RUN case "$TARGETARCH" in \
      amd64) echo x86_64-unknown-linux-gnu > /rust-target ;; \
      arm64) echo aarch64-unknown-linux-gnu > /rust-target ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && rustup target add "$(cat /rust-target)" \
    && if [ "$TARGETARCH" = "arm64" ]; then \
         apt-get update \
         && apt-get install -y --no-install-recommends gcc-aarch64-linux-gnu \
         && rm -rf /var/lib/apt/lists/*; \
       fi
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc

COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin homeostat --target "$(cat /rust-target)" \
    && cp "target/$(cat /rust-target)/release/homeostat" /homeostat

FROM debian:bookworm-slim

# git: plan --save, apply and the MCP repo tools shell out to it.
# tini: PID 1 — forwards signals and reaps orphans left by `uv run` wrappers.
# tzdata: the uv-managed CPython reads /usr/share/zoneinfo for zoneinfo.
RUN apt-get update \
    && apt-get install -y --no-install-recommends git tini tzdata ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    # The mounted house repo is usually owned by a host uid, not root.
    && git config --system --add safe.directory '*'

COPY --from=ghcr.io/astral-sh/uv:0.9 /uv /uvx /usr/local/bin/
ENV UV_PYTHON_INSTALL_DIR=/opt/uv/python \
    UV_CACHE_DIR=/var/cache/uv
# Pre-install the interpreter so first boot doesn't download one. Unit
# dependencies still resolve on first run; mount /var/cache/uv to keep
# them across container replacements.
RUN uv python install 3.12

COPY --from=build /homeostat /usr/local/bin/homeostat

# The supervisor's bus endpoint; units and observers connect here.
EXPOSE 7447
ENTRYPOINT ["/usr/bin/tini", "--", "homeostat"]
CMD ["up", "/house", "--listen", "tcp/0.0.0.0:7447"]
