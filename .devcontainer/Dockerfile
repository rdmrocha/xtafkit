FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    git \
    lldb \
    pkg-config \
    curl \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user first, then install Rust as that user
ARG USERNAME=vscode
ARG USER_UID=1000
ARG USER_GID=$USER_UID
RUN groupadd --gid $USER_GID $USERNAME \
    && useradd --uid $USER_UID --gid $USER_GID -m $USERNAME

# Install Rust as the vscode user so all of ~/.cargo is owned correctly
USER $USERNAME
ENV PATH="/home/$USERNAME/.cargo/bin:${PATH}"
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
    && . "$HOME/.cargo/env" \
    && cargo install cargo-watch cargo-expand

WORKDIR /workspaces/xtafkit
