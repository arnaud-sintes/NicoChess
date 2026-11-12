# Build environment for the Android app: JDK, Android SDK + NDK, Gradle, Rust with
# the Android targets, and cargo-ndk. Nothing needs to be installed on the PC.
#
#   .\scripts\build-android.ps1

FROM debian:12-slim

ENV DEBIAN_FRONTEND=noninteractive \
    ANDROID_HOME=/opt/android-sdk \
    ANDROID_NDK_HOME=/opt/android-sdk/ndk/27.2.12479018 \
    GRADLE_HOME=/opt/gradle-8.10.2 \
    RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo
ENV PATH=/opt/cargo/bin:/opt/gradle-8.10.2/bin:/opt/android-sdk/cmdline-tools/latest/bin:$PATH

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      openjdk-17-jdk-headless build-essential ca-certificates curl unzip file \
 && rm -rf /var/lib/apt/lists/*

# Android command-line tools, then the platform, build tools and NDK the app uses.
RUN mkdir -p $ANDROID_HOME/cmdline-tools \
 && curl -fsSL -o /tmp/cmdline-tools.zip https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip \
 && unzip -q /tmp/cmdline-tools.zip -d $ANDROID_HOME/cmdline-tools \
 && mv $ANDROID_HOME/cmdline-tools/cmdline-tools $ANDROID_HOME/cmdline-tools/latest \
 && rm /tmp/cmdline-tools.zip \
 && (yes | sdkmanager --licenses > /dev/null || true) \
 && sdkmanager "platforms;android-34" "build-tools;34.0.0" "ndk;27.2.12479018"

RUN curl -fsSL -o /tmp/gradle.zip https://services.gradle.org/distributions/gradle-8.10.2-bin.zip \
 && unzip -q /tmp/gradle.zip -d /opt \
 && rm /tmp/gradle.zip

# 64-bit ARM (almost every phone since 2017) and 32-bit ARM (older phones and tablets).
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --profile minimal --default-toolchain stable \
      --target aarch64-linux-android --target armv7-linux-androideabi \
 && cargo install cargo-ndk --locked

WORKDIR /work
