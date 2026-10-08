# The homeostat appliance image. It holds the supervisor binary and the
# runtime a deployed house needs: git for the repo surface, and uv with a
# pre-installed Python for the units. Run it with the house repo mounted at
# /house:
#
#   docker run -v /path/to/house:/house ghcr.io/freol35241/homeostat
#
# The bus port is not published. A client that reaches 7447 has full
# authority over the house (docs/design.md, Local-only access), and nothing
# outside the container network needs it.
#
# The builder stage runs on the build host's architecture and
# cross-compiles for $TARGETARCH, so a multi-arch `docker buildx build`
# does not run rustc under QEMU.

FROM --platform=$BUILDPLATFORM rust:1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS build
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
         && apt-get install -y --no-install-recommends \
              gcc-aarch64-linux-gnu libc6-dev-arm64-cross \
         && rm -rf /var/lib/apt/lists/*; \
       fi
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc

COPY Cargo.toml Cargo.lock ./
COPY src ./src
# The commit this image is built from. It is compiled into the binary's
# `about` (home/meta/system/about), which the dashboard footer shows. The
# release workflow passes it. A local build leaves it empty and reports the
# version alone. It is declared after the COPYs so that a new value only
# re-runs the build step.
ARG HOMEOSTAT_COMMIT=""
RUN cargo build --release --locked --bin homeostat --target "$(cat /rust-target)" \
    && cp "target/$(cat /rust-target)/release/homeostat" /homeostat

# go2rtc is the camera restreamer that the go2rtc shim unit spawns from
# PATH. The image provides it, and the house repo does not
# (docs/design.md, Cameras). It is a static Go binary, fetched per target
# architecture and pinned by checksum.
ARG GO2RTC_VERSION=1.9.14
RUN case "$TARGETARCH" in \
      amd64) sha=32d616af226bd731678ffde328b94cfb94e30339bfefc469cfb76323144615a6 ;; \
      arm64) sha=359fabade8a7a51e81a55fe6df6b0ef81764a5e1d63179577534eaaa71904b50 ;; \
    esac \
    && curl -fsSL -o /go2rtc \
      "https://github.com/AlexxIT/go2rtc/releases/download/v${GO2RTC_VERSION}/go2rtc_linux_${TARGETARCH}" \
    && echo "$sha  /go2rtc" | sha256sum -c - \
    && chmod +x /go2rtc

FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251

# Nothing in here needs root. The supervisor and every unit run as this
# user. 1000 is the first login user on most single-user hosts, so a house
# checkout is usually writable as-is. For any other owner, run the
# container as that uid (`--user`, or `user:` in compose). The runtime
# directories below are set up so that any uid works.
ARG UID=1000
ARG GID=1000

# git: plan --save and apply shell out to it for the house commit.
# tini: PID 1. It forwards signals and reaps any orphan a unit leaves behind.
# tzdata: the uv-managed CPython reads /usr/share/zoneinfo for zoneinfo.
RUN apt-get update \
    && apt-get install -y --no-install-recommends git tini tzdata ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd -g "$GID" homeostat \
    && useradd -m -u "$UID" -g "$GID" homeostat \
    # The mounted house repo may be owned by a different host uid. Mark
    # only that path as safe for git.
    && git config --system --add safe.directory /house

COPY --from=ghcr.io/astral-sh/uv:0.9@sha256:538e0b39736e7feae937a65983e49d2ab75e1559d35041f9878b7b7e51de91e4 /uv /uvx /usr/local/bin/
ENV UV_PYTHON_INSTALL_DIR=/opt/uv/python \
    UV_CACHE_DIR=/var/cache/uv \
    UV_FIND_LINKS=/opt/homeostat-wheels
# The interpreter and the wheel are only read at runtime. The cache is
# written by whichever uid the container runs as (see above), hence 1777.
# The wheel directory must exist before anything runs uv, because uv reads
# UV_FIND_LINKS on every invocation, `uv build` included, and fails if the
# directory is missing.
RUN mkdir -p /opt/uv /var/cache/uv /opt/homeostat-wheels \
    && chown homeostat:homeostat /opt/uv /var/cache/uv /opt/homeostat-wheels \
    && chmod 1777 /var/cache/uv
USER homeostat
# Pre-install the interpreter so first boot doesn't download one. Unit
# dependencies still resolve on first run. Mount /var/cache/uv to keep
# them across container replacements.
RUN uv python install 3.12

# The SDK, as a wheel the units resolve locally. A unit declares
# `homeostat==X.Y.Z` and no [tool.uv.sources]. UV_FIND_LINKS above points
# uv here, so first boot needs no clone and no network for the SDK
# (docs/design.md, SDK distribution).
COPY --chown=homeostat:homeostat sdk/python /tmp/sdk
RUN uv build --wheel /tmp/sdk -o /opt/homeostat-wheels \
    && rm -rf /tmp/sdk \
    # `uv python install` and the build above have already filled
    # /var/cache/uv (sdists, wheels, the interpreter archive) as this
    # build's uid. Each entry has uv's default mode, which gives "other"
    # read and execute but not write.
    # A container run with another uid (--user, or compose's
    # HOMEOSTAT_UID/GID) mounts a named volume over this path. Docker fills
    # a fresh volume from the image, permissions included. That uid can
    # create new top-level entries (the 1777 above) but cannot write inside
    # the entries this build created, and uv fails when it opens a file
    # under one of them.
    # Opening every existing entry to "other" here fixes that for any
    # runtime uid. Entries a running container creates need no fix, since
    # all units in a container share its uid.
    && chmod -R o+rwX /var/cache/uv

COPY --from=build /homeostat /usr/local/bin/homeostat
COPY --from=build /go2rtc /usr/local/bin/go2rtc

# An owner tool that reports which series fill the history store. It reads
# the bus address from HOMEOSTAT_BUS below, so `docker exec <container> uv
# run /opt/homeostat/store_profile.py` needs no arguments.
COPY scripts/store_profile.py scripts/store_profile.py.lock /opt/homeostat/

# The supervisor's bus endpoint. Units and sibling containers connect here
# over the container network. Publishing it to the host gives every client
# that can reach it full authority (see the note at the top).
EXPOSE 7447

# The same endpoint on loopback. With it, `docker exec <container>
# homeostat apply /house` reaches the supervisor running as this
# container's PID 1, and the operator does not have to pass the address.
# An explicit --bus still takes precedence.
# This value is kept in step with the CMD below by hand. If the listen
# address becomes configurable at runtime, both must come from that one
# setting. A stale value here would apply against the wrong bus, which is
# worse than the missing-address error it replaces.
# A one-shot `docker compose run` container also gets this value, but
# nothing listens on 7447 there. `plan --save` in such a container fails
# to connect instead of printing the "pass --bus" message.
ENV HOMEOSTAT_BUS=tcp/127.0.0.1:7447
ENTRYPOINT ["/usr/bin/tini", "--", "homeostat"]
CMD ["up", "/house", "--listen", "tcp/0.0.0.0:7447"]
