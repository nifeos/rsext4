# rsext4

[![check](https://github.com/partsfoundry/rsext4/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/partsfoundry/rsext4/actions/workflows/ci.yml?query=branch%3Amain)
[![proofs](https://github.com/partsfoundry/rsext4/actions/workflows/proofs.yml/badge.svg?branch=main)](https://github.com/partsfoundry/rsext4/actions/workflows/proofs.yml?query=branch%3Amain)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](https://github.com/partsfoundry/rsext4#license)
[![minimum Rust version](https://img.shields.io/badge/rust-1.94%2B-blue.svg)](https://github.com/partsfoundry/rsext4/blob/main/Cargo.toml)

A `no_std` ext4 file system in Rust: mount, read, write, format and journal replay over any block
device that implements `BlockIo`.

This repository is a fork of `rsext4`, which lives in
[rcore-os/tgoskits](https://github.com/rcore-os/tgoskits) under `fs/rsext4`. It tracks that
directory, starting from the published 0.9.2 release, and carries fixes upstream has not released
yet. Each fix goes upstream as an issue and a pull request of its own, written to upstream's
conventions, and the fork stops carrying it once upstream releases it.
[CHANGES.md](CHANGES.md) lists every file the fork changes, why, and where each fix stands
upstream. The history before the fork is upstream's own, split out of tgoskits.

[README_CN.md](README_CN.md) is upstream's Chinese README, unchanged.

## EXAMPLES

Format a device, mount it, and write and read one file.
[examples/host_perf.rs](examples/host_perf.rs) runs this sequence and defines `MemoryDevice` (a
`BlockIo` over a byte vector) and `BenchClock` (a fixed clock).

```rust
use rsext4::{
    Ext4, FileName, FilePermissions, MkfsOptions, MountOptions, MountServices, MutationContext,
    NoopObserver, format,
};

let device = format(MemoryDevice::new(), BenchClock(Cell::new(1_700_000_000)), MkfsOptions::default())?;
let services = MountServices::new(BenchClock(Cell::new(1_700_000_000)), (), NoopObserver);
let mut filesystem = Ext4::mount(device, services, MountOptions::read_write())?;
let file = filesystem.create_regular_file(
    MutationContext::new(0, 0, 0, 0),
    filesystem.root_inode(),
    FileName::new(b"hello.txt")?,
    FilePermissions::new(0o644)?,
)?;
filesystem.write_inode(file.number, 0, b"hello")?;
let mut buffer = [0; 5];
filesystem.read_inode(file.number, 0, &mut buffer)?;
filesystem.unmount()?;
```

## What is proven

Nothing yet. The crate carries no Kani harnesses; adding them is open work.

## Building and testing

```sh
cargo build
cargo test --all-features
rustup target add aarch64-unknown-none-softfloat riscv64imac-unknown-none-elf x86_64-unknown-none
cargo build --lib --target aarch64-unknown-none-softfloat
cargo build --lib --target riscv64imac-unknown-none-elf
cargo build --lib --target x86_64-unknown-none
```

Some tests format images with `mke2fs` and check them with `e2fsck`, so they need e2fsprogs 1.47
or newer on the path. The fuzz targets are in `fuzz/`, and [fuzz/README.md](fuzz/README.md)
says how to build their seed corpus and run them.

## BUGS

- The crate's names, comments and documentation follow upstream's conventions, not those of the
  other crates that share these workflows, because a change written to upstream's conventions can go
  upstream unchanged.

## Minimum Rust version

Rust 1.94. To check it, run `rustup toolchain install 1.94 --profile minimal`, then
`cargo +1.94 test --all-features`, and the bare-metal builds above with `+1.94`.

## License

Licensed under the Apache License, Version 2.0 ([LICENSE](LICENSE)), as upstream licenses it.
