# rsext4

[![check](https://github.com/partsfoundry/rsext4/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/partsfoundry/rsext4/actions/workflows/ci.yml?query=branch%3Amain)
[![proofs](https://github.com/partsfoundry/rsext4/actions/workflows/proofs.yml/badge.svg?branch=main)](https://github.com/partsfoundry/rsext4/actions/workflows/proofs.yml?query=branch%3Amain)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](https://github.com/partsfoundry/rsext4#license)
[![minimum Rust version](https://img.shields.io/badge/rust-1.95%2B-blue.svg)](https://github.com/partsfoundry/rsext4/blob/main/Cargo.toml)

A `no_std` ext4 file system in Rust: mount, read, write, format and journal replay over any block
device that implements `BlockIo`.

This repository is a fork of `rsext4`, which lives in
[rcore-os/tgoskits](https://github.com/rcore-os/tgoskits) under `fs/rsext4`. It tracks that
directory: it started from the published 0.9.2 release, merges upstream's `dev` branch as a
routine step, and carries fixes upstream has not released yet. Each fix goes upstream as an issue and a pull request of its own, written to upstream's
conventions, and the fork stops carrying it once upstream releases it.
[CHANGES.md](CHANGES.md) lists every file the fork changes, why, and where each fix stands
upstream. The history before the fork is upstream's own, split out of tgoskits.

[README_CN.md](README_CN.md) is upstream's Chinese README, unchanged.

## EXAMPLES

Format a device, mount it, and write and read one file.
[examples/host_perf.rs](examples/host_perf.rs) runs this sequence and defines `MemoryDevice` (a
`BlockIo` over a byte vector) and `BenchClock` (a fixed clock).

```rust
use core::cell::Cell;
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

Checksums use the table-driven Castagnoli cyclic redundancy check (CRC32C) code by default, which
runs on any CPU. An environment that can guarantee the CRC32C instructions on every CPU that may
run the crate turns on the hardware path once, before mounting; the documentation of
`enable_hardware_crc32c` names the instructions and where the guarantee can come from.

```rust
// SAFETY: every CPU this program may run on implements the CRC32C instructions.
unsafe { rsext4::crc32c::enable_hardware_crc32c() };
```

## What is proven

Eight Kani harnesses prove the on-disk parsers, each for every input up to the bound it states:

- decoding a superblock, a group descriptor, an extent header, extent or extent index, or a
  directory entry never panics and reads only inside the slice it receives;
- the group descriptor decode accepts exactly the record sizes ext4 defines;
- an extent tree node holds no more entries than fit after its header;
- a directory block walk ends, and every entry it yields lies inside its record and its block;
- the one-byte step of the software CRC32C, and every entry of its slicing tables, equal the
  bitwise definition.

Each harness sits in a `verification` module beside the code it proves, behind `#[cfg(kani)]`,
and each has a patch in [falsifications/](falsifications/) that breaks the property and must turn
the harness red. "Running the proofs" below says how to run them. The bounds are small (24 bytes for the directory and
extent tree walks, 128 for the group descriptor), so a defect that needs a longer input is not
ruled out.

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
or newer on the path. Most Linux distributions ship it as the `e2fsprogs` package; on macOS,
`brew install e2fsprogs` installs it outside the default path. The fuzz targets are in `fuzz/`, and [fuzz/README.md](fuzz/README.md)
says how to build their seed corpus and run them.

### Running the proofs

Install Kani first: run `cargo install --locked kani-verifier --version 0.68.0`, then
`cargo kani setup`. Setup downloads a model checker and the Rust toolchain that Kani pins, so it
needs network access, and `cargo install` compiles Kani from source. Then run every harness:

```sh
cargo kani
```

The eight harnesses take about three and a half minutes on a shared 80-core x86_64 host. Three of
them take almost all of it: `superblock_decode_reads_only_its_record` (about 80 seconds),
`extent_node_parse_is_total` (about 56) and `directory_block_walk_ends_inside_its_block` (about
40). The other five take a few seconds each. To run one harness, give its full path, because
`--exact` refuses a bare name:

```sh
cargo kani --harness entries::verification::directory_entry_parse_is_total --exact
```

The full path of each harness is its patch's file name in `falsifications/` without `.patch`, with
each `.` read as `::`.

A passing run prints lines that look alarming and mean nothing. They are the `use of an unstable
feature` warning, the `Found the following unsupported constructs` warning, checks marked
`unreachable`, and many `aborting path on assume(false)` lines, which are paths Kani discards on
purpose. The answer is the last line,
`Complete - 8 successfully verified harnesses, 0 failures, 8 total.`, and `cargo kani` exits 0.

### Running one falsification patch

Each file in `falsifications/` applies a small defect that turns its harness red. Apply one,
confirm the proof fails, then undo it with the same patch reversed:

```sh
patch -p1 --no-backup-if-mismatch < falsifications/entries.verification.directory_entry_parse_is_total.patch
cargo kani --harness entries::verification::directory_entry_parse_is_total --exact
patch -R -p1 --no-backup-if-mismatch < falsifications/entries.verification.directory_entry_parse_is_total.patch
```

The failing run prints a placeholder for the failed slice index.

```text
Failed Checks: This is a placeholder message; Kani doesn't support message formatted at runtime
```

Kani prints that line for a message built at runtime. Then it prints `VERIFICATION:- FAILED` and
ends with `Complete - 0 successfully verified harnesses, 1 failures, 1 total.`, and `cargo kani`
exits 1. Each patch's opening lines name the check it expects to fail.

## BUGS

- `read_file` reads a whole file into one buffer, holes included, up to the bound its caller
  passes. On a host whose allocator overcommits, the allocator can grant a reservation within that
  bound and run out of memory only when the read fills it; the caller's bound is the control.
  `read_inode_data_into` reads a range into a caller's buffer instead.
- The crate's names, comments and documentation follow upstream's conventions, not those of the
  other crates that share these workflows, because a change written to upstream's conventions can go
  upstream unchanged.

## Minimum Rust version

Rust 1.95. To check it, run:

```sh
rustup toolchain install 1.95 --profile minimal
rustup target add --toolchain 1.95 aarch64-unknown-none-softfloat riscv64imac-unknown-none-elf x86_64-unknown-none
cargo +1.95 test --all-features
```

then the bare-metal builds above with `+1.95`.

## License

Licensed under the Apache License, Version 2.0 ([LICENSE](LICENSE)), as upstream licenses it.
