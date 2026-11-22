# Rexa Desktop

A desktop client for [Rex](https://superlogical.com) on Linux and macOS: terminals that
survive disconnects, with a file panel, a small editor and file transfer for the machines
your Rex servers run on.

Work in progress.

## Build

Needs Rust (see `rust-toolchain.toml`) and, on Linux, Zig 0.17 for libghostty-vt
(`tools/zig` points at `~/.local/share/zig/0.17.1/zig`; set `ZIG` to use another one).

    cargo build --release
