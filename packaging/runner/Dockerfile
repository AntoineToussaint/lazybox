# Build context is the Linux release directory produced by scripts/release/build.mjs.
FROM ubuntu:24.04@sha256:73b999393d07f2bf65a40373778a44dc9a6d54ad7718d29247d8ff9413c4d6a8

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git libatomic1 openssh-client \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --user-group --home-dir /var/lib/lazybox --create-home lazybox

ARG TARGETARCH
ARG LAZYBOX_VERSION
ARG LAZYBOX_BUNDLE_SHA256
RUN --mount=type=bind,target=/tmp/bundles set -eu; \
    case "$TARGETARCH" in amd64) release_arch=x64 ;; arm64) release_arch=arm64 ;; *) exit 1 ;; esac; \
    bundle="/tmp/bundles/lazybox-runner-${LAZYBOX_VERSION}-linux-${release_arch}.tar.gz"; \
    printf '%s  %s\n' "$LAZYBOX_BUNDLE_SHA256" "$bundle" | sha256sum --strict --check; \
    mkdir -p /opt/lazybox; \
    tar -xzf "$bundle" --no-same-owner -C /opt/lazybox; \
    (cd /opt/lazybox && sha256sum --quiet --strict --check SHA256SUMS)

ENV XDG_DATA_HOME=/var/lib/lazybox/data \
    XDG_CONFIG_HOME=/var/lib/lazybox/config \
    XDG_STATE_HOME=/var/lib/lazybox/state \
    PATH=/opt/lazybox/bin:$PATH

WORKDIR /var/lib/lazybox
USER 10001:10001
ENTRYPOINT ["/opt/lazybox/bin/lazybox-runner", "serve"]
