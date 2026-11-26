# Changes this fork makes to rsext4

This repository modifies `rsext4` as published by rcore-os/tgoskits under `fs/rsext4` at tag
`rsext4-v0.9.2` (commit `bd0153f7d336e3106ea6fd78b3a7ef2fe256d906`). The Apache License,
Version 2.0, section 4(b), asks that modified files carry a prominent notice that they were
changed. This file is that notice: it lists every file the fork changes or adds, and why. A file
not listed here is upstream's, unchanged.

## Fixes submitted upstream

Each fix is one commit, written to upstream's conventions so it can go upstream unchanged, and
each goes upstream as an issue and a pull request of its own. "Not yet filed" means the upstream
issue or pull request does not exist yet.

Upstream has merged fixes 2a, 4, 5a, 5b, 7 and 9, but no release carries them yet: the latest rsext4 on
crates.io is still 0.9.2, the release this fork starts from. The fork keeps each merged fix until an
upstream release carries it.

| fix | files | what it fixes | upstream |
|---|---|---|---|
| 2a | `src/extents_tree/remove.rs`, `tests/extent_truncate.rs` | shrinking an extent-mapped file past a whole index node wrote the emptied node back instead of freeing it | [issue 2826](https://github.com/rcore-os/tgoskits/issues/2826), [pull request 2827](https://github.com/rcore-os/tgoskits/pull/2827), merged |
| 2b | `src/file/io.rs`, `tests/extent_restart.rs`, `tests/extent_truncate.rs` | a truncate that failed after zeroing the tail of the retained block left the tail zeroed; a single-transaction truncate now restores the tail, also when the write that zeroes it fails, and reports a restore that fails in place of the truncate's error, with the operation `rollback:truncate_tail_restore`; a truncate too large for one transaction commits its new size and orphan record before it zeroes the tail, so a failure or a crash before that commit leaves the old size with its tail intact | [issue 2828](https://github.com/rcore-os/tgoskits/issues/2828), [pull request 2829](https://github.com/rcore-os/tgoskits/pull/2829), open |
| 2c | `src/file/io.rs`, `tests/extent_restart.rs`, `tests/extent_truncate.rs` | a truncate that fits in one transaction zeroed the tail of the retained block in place and left its new size in the running transaction, so a failed or interrupted commit of that transaction brought back the old size over a zeroed tail; a user truncate that zeroes part of a block now commits its new size and an orphan record first, and orphan recovery finishes the shrink after a later failure, so the undo copy of the tail is removed | [issue 2844](https://github.com/rcore-os/tgoskits/issues/2844), [pull request 2845](https://github.com/rcore-os/tgoskits/pull/2845), open |
| 4 | `fuzz/` (all files) | adds cargo-fuzz targets for the superblock, the group descriptor table, extent tree nodes, directory blocks, HTree lookup, mount and walk, and journal replay | [issue 2834](https://github.com/rcore-os/tgoskits/issues/2834), [pull request 2835](https://github.com/rcore-os/tgoskits/pull/2835), merged |
| 5a | `src/crc32c/arm64.rs`, `src/crc32c/crc32c.rs`, `src/crc32c/mod.rs`, `src/crc32c/x86_64.rs` | the hardware CRC32C path was chosen by a feature check cached from the first CPU to run it, which says nothing about the CPU a later call runs on; the table-driven code is now the default, and the environment turns the hardware path on with `enable_hardware_crc32c`, an `unsafe` function whose caller guarantees the instructions on every CPU; each `unsafe` use states its safety argument | [issue 2830](https://github.com/rcore-os/tgoskits/issues/2830), [pull request 2831](https://github.com/rcore-os/tgoskits/pull/2831), merged |
| 5b | `src/blockgroup_description/table.rs`, `src/blockgroup_description/tests.rs`, `src/test_contracts.rs` | group descriptors were read by casting a byte offset in the table to `&Ext4GroupDesc`, which is undefined behavior on unaligned bytes, reads native byte order and always spans 64 bytes, so 32-byte descriptors read and wrote the next record; each record is now decoded at its own size (breaking: `get_desc` returns `Option<Ext4GroupDesc>`, and `set_desc` replaces `get_desc_mut`) | [issue 2832](https://github.com/rcore-os/tgoskits/issues/2832), [pull request 2833](https://github.com/rcore-os/tgoskits/pull/2833), merged |
| 6 | `Cargo.toml` (`[lints.rust]`), `src/blockgroup_description/mod.rs`, `src/blockgroup_description/verification.rs`, `src/crc32c/crc32c.rs`, `src/crc32c/crc32c/verification.rs`, `src/disknode/mod.rs`, `src/disknode/verification.rs`, `src/entries/mod.rs`, `src/entries/verification.rs`, `src/extents_tree/mod.rs`, `src/extents_tree/verification.rs`, `src/superblock/mod.rs`, `src/superblock/verification.rs`, `falsifications/` (all files) | adds Kani harnesses for the on-disk parsers and the software CRC32C, each with a falsification patch, and changes no other code | [issue 2836](https://github.com/rcore-os/tgoskits/issues/2836), [pull request 2846](https://github.com/rcore-os/tgoskits/pull/2846), open |
| 7 | `src/ext4/mount.rs`, `src/ext4/mkfs.rs`, `tests/error_handling.rs` | mount mapped every error from reading the superblock to a failed device read; it now reports the error itself, such as a block size the device cannot address, and still refuses a device with no ext4 file system as an invalid magic number | [issue 2837](https://github.com/rcore-os/tgoskits/issues/2837), [pull request 2839](https://github.com/rcore-os/tgoskits/pull/2839), merged |
| 8 | `src/ext4/owned.rs`, `tests/integration_test.rs` | `InodeInfo` dropped the nanoseconds and the extra epoch bits of the inode times; `atime`, `ctime` and `mtime` are now `Ext4Timestamp`, and `btime` is `Option<Ext4Timestamp>` (breaking) | [issue 2838](https://github.com/rcore-os/tgoskits/issues/2838), [pull request 2840](https://github.com/rcore-os/tgoskits/pull/2840), open |
| tests | `src/bitmap/error.rs`, `src/cache/bitmap.rs`, `src/cache/inode_table.rs`, `src/crc32c/crc32c.rs`, `src/disknode/mod.rs`, `src/error/context.rs`, `src/extents_tree/convert.rs`, `src/file/mod.rs`, `src/hashtree/tests.rs`, `src/io.rs`, `tests/directory_entry_creation.rs`, `tests/hardware_crc32c_switch.rs`, `tests/owned_boundary.rs` | adds tests, and changes no other code, so every file reaches a line coverage of 85 percent: error text, the bitmap and inode table caches, the CRC32C table generator, device numbers, directory entry insertion, `mkdir` and file creation refusals, extent conversion on deep trees, and the owned API's directory cursors and special inodes | not yet filed |
| 9 | `src/ext4/alloc.rs` | initializing an inode bitmap for an uninitialized group left the padding bits past the group's last inode clear, which e2fsck rejects | [issue 2824](https://github.com/rcore-os/tgoskits/issues/2824), [pull request 2825](https://github.com/rcore-os/tgoskits/pull/2825), merged |
| 10 | `src/crc32c/crc32c.rs`, `src/crc32c/x86_64.rs` | the x86_64 `crc32c_hardware` required SSE4.2 only on the CPU making the call, though a thread can move while the call runs, and now requires it on every CPU that may execute the call; the `unsafe extern` block of the x86_64 hardware test now states that its declarations match the C prototypes; the aarch64 hardware test argued its precondition only for Linux and macOS and is now built only on Linux, Android and macOS | not yet filed |
| 11 | `src/crc32c/arm64.rs` | the aarch64 `crc32c_hardware` read its input through raw pointer loads in native byte order, so the checksum was wrong on a big-endian target; it now reads 8-, 4-, 2- and 1-byte pieces with `as_chunks`, `split_first_chunk` and `from_le_bytes`, as the x86_64 helper does, and has no `unsafe` block | not yet filed |
| 12 | `src/crc32c/arm64.rs` | the safe `has_hardware_crc32` ran `mrs` on an ID register, which ends the process at EL0 where the kernel does not emulate the read, and nothing called it but the deprecated `is_hardware_crc32_supported`; both are removed (breaking) | not yet filed |
| 13 | `Cargo.toml` (`[lints.rust]`, `[lints.clippy]`), `src/lib.rs` | the crate root denies `unsafe_code` and allows it only in the `crc32c` module, and `unsafe_op_in_unsafe_fn`, `clippy::missing_safety_doc` and `clippy::undocumented_unsafe_blocks` are set to `deny` explicitly | not yet filed |
| 14 | `src/crc32c/crc32c.rs` | the x86_64 hardware test declares `sched_getcpu` as `safe fn` in its `unsafe extern` block, so its call needs no `unsafe` | not yet filed |

## Fixes taken from upstream after 0.9.2

Upstream fixed these after the release this fork starts from, inside larger changes this fork does
not take. Each row is the smallest part of upstream's change that fixes the fault, with upstream's
test.

| files | what it fixes | upstream |
|---|---|---|
| `src/hashtree/mutation.rs`, `tests/directory_operations.rs` | growing an indexed directory by one block set the inode's block count after the extent tree had added its leaf and index blocks to it, so the count left them out and e2fsck reported a short `i_blocks`; the data block is now counted before the extent is inserted | part of commit `dde4020be` ([pull request 2525](https://github.com/rcore-os/tgoskits/pull/2525)), merged |

## Changes only this fork needs

These are not meant for upstream.

| files | why |
|---|---|
| `Cargo.toml` | the workspace-inherited `edition` and `thiserror` keys are resolved to literal values, so the crate builds outside the tgoskits workspace; the version is `0.10.0-nifeos.1`, a pre-release that cannot collide with an upstream release; `rust-version` is set; `repository` names this repository, and `documentation` is dropped |
| `.github/workflows/` | upstream's `check`, `deploy`, `release` and `test` workflows are removed, since they publish to crates.io and to documentation pages as part of the tgoskits release; the shared crate workflows replace them, and `ci.yml` adds a job that runs the tests with default features off, the profile upstream pull request 2829 adds to tgoskits, and a job on a hosted arm64 runner that runs the aarch64 hardware CRC32C tests, which no shared job runs |
| `tests/hardware_crc32c_random.rs` | added: checks the aarch64 hardware CRC32C path against the table-driven path on random inputs, at every alignment and every length modulo eight, for the arm64 job of `ci.yml` |
| `.github/` (the other files), `.githooks/`, `docs/stranger-test.md`, `CODEOWNERS`, `CONTRIBUTING.md`, `SECURITY.md`, `_typos.toml`, `deny.toml`, `rustfmt.toml` | added: the configuration of the shared crate workflows |
| `docs/unsafe-review.md` | added: a review of every use of `unsafe` in the crate and of each safety argument, as the input to a maintainer's review by hand |
| `README.md` | rewritten to say what the fork is and how to build and test it |
| `CHANGES.md` | added: this file |
| `LICENSE-APACHE` | added: a copy of upstream's `LICENSE`, the Apache License, Version 2.0, under the name the shared crate workflows read; `LICENSE` stays as upstream's |

The repository's topics are `rust`, `no-std` and `filesystem`. The shared settings also give every crate repository `formal-verification` and `kani`, which apply here now that the crate carries Kani harnesses.
