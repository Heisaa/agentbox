FROM rust:1.96-bookworm AS rust-toolchain

RUN rustup component add rustfmt clippy

FROM node:24-bookworm

ARG TARGETARCH

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash \
        bubblewrap \
        build-essential \
        ca-certificates \
        curl \
        7zip \
        fd-find \
        git \
        jq \
        fonts-liberation \
        libasound2 \
        libgbm1 \
        libgtk-3-0 \
        libnotify4 \
        libnss3 \
        libx11-xcb1 \
        libxss1 \
        libxtst6 \
        libssl-dev \
        pkg-config \
        postgresql-client \
        python3 \
        python3-pip \
        redis-tools \
        ripgrep \
        socat \
        tini \
        unzip \
    && npm install --global pnpm@latest @anthropic-ai/claude-code @openai/codex \
    && ln -s /usr/bin/fdfind /usr/local/bin/fd \
    && rm -rf /var/lib/apt/lists/*

RUN groupmod --new-name agent node \
    && usermod --login agent --home /home/agent --move-home node \
    && install -d -o agent -g agent -m 0755 /home/agent/.cargo/registry

COPY --from=rust-toolchain /usr/local/cargo /usr/local/cargo
COPY --from=rust-toolchain /usr/local/rustup /usr/local/rustup
RUN for tool in cargo cargo-clippy cargo-fmt clippy-driver rust-analyzer rustc rustdoc rustfmt rustup; do \
        ln -s "/usr/local/cargo/bin/$tool" "/usr/local/bin/$tool"; \
    done
COPY --chmod=755 images/fullstack/entrypoint.sh /usr/local/bin/agentbox-entrypoint

ENV HOME=/home/agent
ENV CARGO_HOME=/home/agent/.cargo
ENV RUSTUP_HOME=/usr/local/rustup
ENV PATH=/home/agent/.local/bin:/home/agent/.cargo/bin:/usr/local/cargo/bin:$PATH

WORKDIR /workspace
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/agentbox-entrypoint"]
CMD ["bash"]
