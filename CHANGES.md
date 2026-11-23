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

| fix | files | what it fixes | upstream |
|---|---|---|---|
| 2a | `src/extents_tree/remove.rs`, `tests/extent_truncate.rs` | shrinking an extent-mapped file past a whole index node wrote the emptied node back instead of freeing it | [issue 2826](https://github.com/rcore-os/tgoskits/issues/2826), [pull request 2827](https://github.com/rcore-os/tgoskits/pull/2827) |
| 2b | `src/file/io.rs`, `tests/extent_truncate.rs` | a single-transaction truncate that failed after zeroing the tail of the retained block left the tail zeroed; the tail is now restored, also when the write that zeroes it fails, and a restore that fails is reported in place of the truncate's error, with the operation `rollback:truncate_tail_restore` | [issue 2828](https://github.com/rcore-os/tgoskits/issues/2828), [pull request 2829](https://github.com/rcore-os/tgoskits/pull/2829) |
| 4 | `fuzz/` (all files) | adds cargo-fuzz targets for the superblock, the group descriptor table, extent tree nodes, directory blocks, HTree lookup, mount and walk, and journal replay | [issue 2834](https://github.com/rcore-os/tgoskits/issues/2834), [pull request 2835](https://github.com/rcore-os/tgoskits/pull/2835) |
| 5a | `src/crc32c/arm64.rs`, `src/crc32c/crc32c.rs` | states the safety argument at each `unsafe` use in the CRC32C code, including that the hardware check runs once and every CPU must give the same answer | [issue 2830](https://github.com/rcore-os/tgoskits/issues/2830), [pull request 2831](https://github.com/rcore-os/tgoskits/pull/2831) |
| 5b | `src/blockgroup_description/table.rs`, `src/blockgroup_description/tests.rs`, `src/test_contracts.rs` | group descriptors were read by casting a byte offset in the table to `&Ext4GroupDesc`, which is undefined behavior on unaligned bytes, reads native byte order and always spans 64 bytes, so 32-byte descriptors read and wrote the next record; each record is now decoded at its own size (breaking: `get_desc` returns `Option<Ext4GroupDesc>`, and `set_desc` replaces `get_desc_mut`) | [issue 2832](https://github.com/rcore-os/tgoskits/issues/2832), [pull request 2833](https://github.com/rcore-os/tgoskits/pull/2833) |
| 7 | `src/ext4/mount.rs`, `src/ext4/mkfs.rs`, `tests/error_handling.rs` | mount mapped every error from reading the superblock to a failed device read; it now reports the error itself, such as a block size the device cannot address, and still refuses a device with no ext4 file system as an invalid magic number | [issue 2837](https://github.com/rcore-os/tgoskits/issues/2837), pull request not yet filed |
| 8 | `src/ext4/owned.rs`, `tests/integration_test.rs` | `InodeInfo` dropped the nanoseconds and the extra epoch bits of the inode times; `atime`, `ctime` and `mtime` are now `Ext4Timestamp`, and `btime` is `Option<Ext4Timestamp>` (breaking) | [issue 2838](https://github.com/rcore-os/tgoskits/issues/2838), pull request not yet filed |
| 9 | `src/ext4/alloc.rs` | initializing an inode bitmap for an uninitialized group left the padding bits past the group's last inode clear, which e2fsck rejects | [issue 2824](https://github.com/rcore-os/tgoskits/issues/2824), [pull request 2825](https://github.com/rcore-os/tgoskits/pull/2825) |

## Changes only this fork needs

These are not meant for upstream.

| files | why |
|---|---|
| `Cargo.toml` | the workspace-inherited `edition` and `thiserror` keys are resolved to literal values, so the crate builds outside the tgoskits workspace; the version is `0.10.0-nifeos.1`, a pre-release that cannot collide with an upstream release; `rust-version` is set; `repository` names this repository, and `documentation` is dropped |
| `.github/workflows/` | upstream's `check`, `deploy`, `release` and `test` workflows are removed, since they publish to crates.io and to documentation pages as part of the tgoskits release; the shared crate workflows replace them |
| `.github/` (the other files), `.githooks/`, `docs/stranger-test.md`, `CODEOWNERS`, `CONTRIBUTING.md`, `SECURITY.md`, `_typos.toml`, `deny.toml`, `rustfmt.toml` | added: the configuration of the shared crate workflows |
| `README.md` | rewritten to say what the fork is and how to build and test it |
| `CHANGES.md` | added: this file |
| `LICENSE-APACHE` | added: a copy of upstream's `LICENSE`, the Apache License, Version 2.0, under the name the shared crate workflows read; `LICENSE` stays as upstream's |

The repository's topics are `rust`, `no-std` and `filesystem`. The shared settings also give every crate repository `formal-verification` and `kani`, which this one leaves out until it carries Kani harnesses; after the shared settings are applied here again, remove those two topics.
