# A box: the gongoozler CLI, uv, Python 3.13, the gongoozler library and its fixed income engine
# as wheels, and a warm package cache with Nautilus in it. A worker runs each job in a container of
# this image (`gongoozler worker --docker`); the server and the workers can run in it too
# (tools/e2e-docker.sh).
#
#   docker build -t gongoozler .

FROM golang:1.25 AS cli
WORKDIR /src
COPY go.mod ./
COPY cmd cmd
COPY internal internal
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /out/gongoozler ./cmd/gongoozler

# The engine as a wheel, compiled once in a stage of its own: Rust, at the toolchain the engine pins.
FROM rust:1-bookworm AS engine
COPY --from=ghcr.io/astral-sh/uv:0.12.10 /uv /usr/local/bin/
COPY engine /src/engine
RUN UV_CACHE_DIR=/tmp/uv-build-cache uv build --wheel /src/engine -o /out && rm -rf /tmp/uv-build-cache

FROM python:3.13-slim
COPY --from=ghcr.io/astral-sh/uv:0.12.10 /uv /uvx /usr/local/bin/
COPY --from=cli /out/gongoozler /usr/local/bin/gongoozler
ENV UV_CACHE_DIR=/opt/uv-cache \
    UV_LINK_MODE=copy \
    UV_PYTHON_DOWNLOADS=never \
    GONGOOZLER_STATE=/state
# The library as a wheel, built with a cache of its own: the box user's cache stays the box user's.
COPY python /tmp/gongoozler-python
RUN UV_CACHE_DIR=/tmp/uv-build-cache uv build --wheel /tmp/gongoozler-python -o /opt/gongoozler/wheels \
    && rm -rf /tmp/gongoozler-python /tmp/uv-build-cache
COPY --from=engine /out/ /opt/gongoozler/wheels/
# A run file names "gongoozler" (or "gongoozler[fi]") among its dependencies; in a box it resolves
# to these wheels.
ENV UV_FIND_LINKS=/opt/gongoozler/wheels
COPY docker/warm.py /opt/gongoozler/warm.py
RUN useradd --create-home --uid 1000 box && mkdir -p /job /state /opt/uv-cache && chown box /job /state /opt/uv-cache
USER box
# Nautilus, gongoozler and the engine into uv's cache, so a run that needs them starts without
# downloading.
RUN uv run --script /opt/gongoozler/warm.py
WORKDIR /job
ENTRYPOINT ["gongoozler"]
