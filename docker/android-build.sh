#!/usr/bin/env bash
# Runs inside the container made from docker/android.Dockerfile (see scripts/build-android.ps1).
# The project is mounted read-only at /src; the work happens on a copy, because
# Gradle and Cargo write a lot of files and bind mounts from Windows are slow.
set -euo pipefail

ABIS="${ABIS:-arm64-v8a armeabi-v7a}"

cd /src
tar --exclude=./.git --exclude=./target --exclude=./dist --exclude=./downloads --exclude=./usb \
    --exclude=./android/native/target --exclude=./android/app/build --exclude=./android/.gradle \
    --exclude=./android/app/src/main/jniLibs \
    -cf - . | tar -xf - -C /work

cd /work/android/native
targets=()
for abi in $ABIS; do targets+=(-t "$abi"); done
cargo ndk --platform 24 "${targets[@]}" -o /work/android/app/src/main/jniLibs build --release

cd /work/android
gradle --no-daemon --console=plain assembleDebug

apk=app/build/outputs/apk/debug/app-debug.apk
cp "$apk" /out/nicochess-debug.apk
ls -l /out/nicochess-debug.apk
unzip -l "$apk" | grep -E "\.so$" || true
