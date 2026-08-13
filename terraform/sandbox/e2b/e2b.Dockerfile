FROM e2bdev/base:latest

USER root
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential ca-certificates clang curl git libc++-dev libc++abi-dev \
    nodejs npm openssh-server pkg-config sudo tmux xz-utils \
    && rm -rf /var/lib/apt/lists/*
RUN npm install --global @anthropic-ai/claude-code
RUN curl -fsSL -o /usr/local/bin/websocat \
    https://github.com/vi/websocat/releases/latest/download/websocat.x86_64-unknown-linux-musl \
    && chmod 0755 /usr/local/bin/websocat

COPY . /opt/lazybox/src
RUN chown -R user:user /opt/lazybox
RUN sudo -u user -H bash -lc \
    'command -v cargo >/dev/null || curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable; cd /opt/lazybox/src && make setup && make release'
RUN install -m0755 /opt/lazybox/src/target/release/lazybox /usr/local/bin/lazybox \
    && install -m0755 /opt/lazybox/src/contrib/box-lifecycle/lazybox-build.sh /usr/local/bin/lazybox-build.sh \
    && install -m0755 /opt/lazybox/src/terraform/sandbox/e2b/start.sh /usr/local/bin/lazybox-e2b-start \
    && install -d /etc/lazybox /run/sshd \
    && (cd /opt/lazybox/src && git rev-parse HEAD > /etc/lazybox/build-sha || printf 'unknown\n' > /etc/lazybox/build-sha)

CMD ["/usr/local/bin/lazybox-e2b-start"]
