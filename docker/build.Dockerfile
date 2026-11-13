# Build environment that matches the target: Debian 12, 32-bit x86.
# The binary built here links against the same glibc as the target's Debian 12.
#
# Why a container: the `v4l` crate generates its V4L2 bindings at build time
# (bindgen), which needs libclang and the Linux kernel headers. They are not
# available on Windows.

# The platform is also set by --platform on the docker commands in scripts/build-target.ps1.
FROM --platform=linux/386 i386/debian:12

ENV DEBIAN_FRONTEND=noninteractive \
    RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    PATH=/opt/cargo/bin:$PATH

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential ca-certificates curl pkg-config clang libclang-dev linux-libc-dev file \
 && rm -rf /var/lib/apt/lists/*

# `linux32` makes `uname -m` report a 32-bit machine (a 64-bit kernel would
# otherwise make rustup pick the 64-bit toolchain), and the host is set explicitly.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | linux32 sh -s -- -y --profile minimal --default-toolchain 1.98.1 \
      --default-host i686-unknown-linux-gnu

WORKDIR /src
