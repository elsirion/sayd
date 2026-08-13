# sayd

Tray-resident local text-to-speech for sway/Wayland. Kokoro-82M via ONNX
Runtime with the misaki-en G2P frontend. No network at runtime.

## Build

    nix develop
    cargo build --release

## Models

    ./scripts/fetch-models.sh

Downloads Kokoro-82M ONNX weights and voice packs into `models/`.

## Status

M1: engine and audio. Speaks text passed on the command line.
D-Bus, CLI, tray, MPRIS and settings are M2-M4.
