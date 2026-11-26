# Review of the unsafe code

This is a review of every use of `unsafe` in the crate, made at main commit `388ebd6` with the
fixes of the same pull request applied. Line numbers are those of the tree that carries this file.
It is the input to a maintainer's own review by hand, not a replacement for one: the section
"What this review could not verify" lists what was reasoned about but not run or proved.

## Terms

A cyclic redundancy check (CRC) is a checksum, and CRC32C (cyclic redundancy check, Castagnoli) is
the 32-bit one ext4 uses for metadata. On x86_64, the `sse4.2` feature adds the `crc32`
instruction, which software detects with the `cpuid` instruction through leaf 1, bit 20 of
register `ecx`. The REX (register extension) prefix encodes 64-bit operands and registers past the
first eight. On aarch64, version 8.0 of the Arm architecture makes the CRC32 instructions optional
and version 8.1 makes them mandatory, and EL0 (exception level 0) runs user programs while EL1
(exception level 1) runs the kernel.

## Where unsafe code lives

Every `unsafe` keyword in `src/`, `tests/`, `fuzz/` and `examples/` is in the CRC32C code
(`src/crc32c/`) or in its tests. `fuzz/` and `examples/` hold none, and there is no `unsafe impl`,
`unsafe trait`, `static mut`, `#[no_mangle]` or `#[repr(packed)]` anywhere.
`src/hashtree/lookup.rs` forbids unsafe code. The `fuzz_target!` macro of `libfuzzer-sys` expands
to an `unsafe extern "C"` entry point; that is the dependency's code, not reviewed here.

No code reinterprets bytes as a struct. A search for `transmute`, `from_raw_parts`,
`read_unaligned`, `write_unaligned`, `as *const`, `as *mut`, `&*`, `ptr::`, `addr_of`,
`MaybeUninit`, `zeroed`, `align_to`, `get_unchecked` and `set_len` finds only the aligned loads in
`src/crc32c/arm64.rs` (rows 7 to 10 below), address arithmetic on `as_ptr() as usize` in the Kani
harnesses of `src/entries/verification.rs`, and pointer comparisons in a test of
`src/jbd2/jbd2.rs`; none of the last two dereferences a pointer. The `#[repr(C)]` on-disk structs
are decoded and encoded field by field through `from_le_bytes` and `to_le_bytes`.

## The group descriptor casts are gone

Group descriptors were once read by casting a byte offset in the table to `&Ext4GroupDesc`.
`src/blockgroup_description/table.rs` now takes each record's byte range with checked arithmetic
(`record_range`), decodes it with `Ext4GroupDesc::decode_checked` at the descriptor size the
superblock gives, and writes it back with `encode_checked`. `get_desc_mut` no longer exists, and
the crate has no pointer cast left that could produce a reference to a descriptor.

## Each use

| # | location | what it relies on | verdict |
|---|---|---|---|
| 1 | `src/crc32c/x86_64.rs:15` `unsafe fn crc32c_hardware` | `# Safety`: every CPU that may execute the call implements `sse4.2`. The function reads `data` only through safe slice operations, so the CPU feature is its whole precondition. | argument wrong, fixed: it named only the CPU making the call, which a thread can leave while the call runs |
| 2 | `src/crc32c/x86_64.rs:20` `asm!` `crc32 r64, r64` | The caller's `sse4.2` guarantee. Both operands are registers, so `nomem` and `nostack` hold, and CRC32 changes no flags (the Intel manual for CRC32 lists "Flags Affected: None"), so `preserves_flags` holds. | holds |
| 3 | `src/crc32c/x86_64.rs:34` `asm!` `crc32 r32, r8` | As row 2. `reg_byte` on x86_64 never picks `ah` to `dh`, which cannot be encoded with the REX prefix the instruction may need. | holds |
| 4 | `src/crc32c/arm64.rs:54` `asm!` `mrs ID_AA64ISAR0_EL1` in the safe `has_hardware_crc32` | The read touches no memory. At EL1 it is architectural; at EL0 it traps, and Linux emulates the ID registers for user space (`HWCAP_CPUID`). | holds; a host that does not emulate the read (macOS) delivers an illegal instruction, which ends the process but is not undefined behavior; the function is public, but only the deprecated `is_hardware_crc32_supported` calls it, and the checksum path does not |
| 5 | `src/crc32c/arm64.rs:84` `unsafe fn crc32c_hardware`, `#[target_feature(enable = "crc")]` | `# Safety`: it runs only on CPUs with the CRC instructions. | holds |
| 6 | `src/crc32c/arm64.rs:96` prefix loop, `*p` and `p.add(1)` | `len > 0`, so `p` points at a byte of `data`; `len` counts the bytes left, so `p.add(1)` stays within `data` or one past its end. | holds |
| 7 | `src/crc32c/arm64.rs:107` `*(p as *const u64)` | The prefix loop leaves `len == 0` or `p` 8-byte aligned; the loop runs only with `len >= 8`, so `p` is aligned and eight bytes lie within `data`. The pointer comes from `data.as_ptr()`, so its provenance covers the read, and every `u8` bit pattern is a valid `u64` byte. | holds |
| 8 | `src/crc32c/arm64.rs:121` `*(p as *const u32)` | Reached only with `len >= 4`, so the prefix loop ended on alignment, and `p` is still 8-byte aligned after the main loop. | holds |
| 9 | `src/crc32c/arm64.rs:133` `*(p as *const u16)` | `p` is an 8-byte boundary plus 0 or 4, so 2-byte aligned; `len >= 2`. | holds |
| 10 | `src/crc32c/arm64.rs:145` last byte, `*p` | `len > 0`, so `p` points at the last byte of `data`. | holds |
| 11 | `src/crc32c/crc32c.rs:126` `unsafe fn enable_hardware_crc32c` | `# Safety`: every CPU that may run the crate's code from the call on implements the instructions. The function only stores a flag. `Relaxed` is enough: a CPU that has not yet seen the store keeps the table-driven path, which needs nothing. | holds |
| 12 | `src/crc32c/crc32c.rs:143` call of `crc32c_hardware` in `crc32c_append` | The flag is set only by row 11, whose caller guarantees the instructions on every CPU, which is the contract of rows 1 and 5. | holds |
| 13 | `src/crc32c/crc32c.rs:316` aarch64 test, call of `crc32c_hardware` | The test returns early unless `is_aarch64_feature_detected!("crc")`. On Linux and Android std reads `HWCAP_CRC32` from `AT_HWCAP`, which the kernel sets from the value it sanitizes across all CPUs and refuses to bring up a later CPU without; on macOS every CPU is version 8.1 of the Arm architecture or later. | argument missing, fixed: the test was built on every aarch64 host but argued only Linux and macOS; it is now built only on Linux, Android and macOS |
| 14 | `src/crc32c/crc32c.rs:360` `unsafe extern "C"` block | The declarations must match the C prototypes: `int sched_getcpu(void)` and `int sched_setaffinity(pid_t, size_t, const cpu_set_t *)`, with `int` and `pid_t` 32-bit signed and `size_t` equal to `usize` on x86_64 Linux. They do, and the 16-word `CpuSet` is the 1024-bit `cpu_set_t` of glibc and musl. | argument missing, fixed: the block had no comment |
| 15 | `src/crc32c/crc32c.rs:378` `sched_getcpu()` | No arguments; it only reads state. A negative return fails `usize::try_from`, and the test skips. | holds |
| 16 | `src/crc32c/crc32c.rs:392` `sched_setaffinity(0, size_of::<CpuSet>(), &mask)` | `mask` is a live `CpuSet` of the size passed, which the kernel only reads; pid 0 is the calling thread. A bit past the set is refused before the call. | holds |
| 17 | `src/crc32c/crc32c.rs:412` x86_64 test, call of `crc32c_hardware` | The thread is pinned to one CPU, and `cpuid` leaf 1, `ecx` bit 20 read after pinning reported `sse4.2`. If the thread had moved between `sched_getcpu` and `sched_setaffinity`, Linux migrates it to the one allowed CPU before the call returns. The comment states the remaining assumption: no CPU hot-unplug, cpuset change or outside affinity change during the test. Breaking it would raise an illegal instruction, not corrupt memory. | holds, under the stated assumption |
| 18 | `tests/hardware_crc32c_switch.rs:15` call of `enable_hardware_crc32c` | The binary holds this one test, and nothing in it computes a checksum after the call, so no CPU runs the instructions the call allows. | holds; it stops holding if a test that computes a checksum is added to this binary, as the file's header says |

Of 18 uses, 15 hold as written, one argument was wrong and two were missing; this pull request
fixes all three.

## Lints

- `unsafe_op_in_unsafe_fn`: not set in `Cargo.toml`, but the crate is edition 2024, where the lint
  warns by default, and the shared `clippy` job runs with `-D warnings`, so it is an error in CI.
  Both `unsafe fn` bodies wrap every unsafe operation in an `unsafe` block.
- `clippy::undocumented_unsafe_blocks`: not enabled. Run by hand (`-W
  clippy::undocumented_unsafe_blocks -W clippy::missing_safety_doc`) it reports nothing for
  `--all-targets` on x86_64 Linux and for `--lib` on `aarch64-unknown-none-softfloat` and
  `x86_64-unknown-none`. The lint accepts the `// SAFETY:` comment as the first line inside the
  block, which is the placement upstream uses. It does not check `unsafe extern` blocks, which is
  how row 14 had no comment without a warning.
- `clippy::multiple_unsafe_ops_per_block` would flag rows 6 to 10, which each dereference and
  advance a pointer in one block under one comment. That is a style choice, not a missing argument.

## What this review could not verify

- This review reasoned about the aarch64 code (rows 4 to 10 and 13) and linted it for
  `aarch64-unknown-none-softfloat`, but did not run it: the CI runners and the reviewing host are
  x86_64, and no aarch64 emulator was at hand. Its tests run only on an aarch64 host.
- The reviewing host lacks the aarch64 test target with std, so only an x86_64 build checked the
  syntax of the `#[cfg]` change in row 13.
- Rows 13 and 17 rely on Linux behavior (the kernel sanitizes `HWCAP_CRC32` across CPUs and
  refuses a late CPU without it; `sched_setaffinity` migrates the thread before it returns) that
  this review takes from the kernel's documented behavior and did not test.
- Miri runs weekly on x86_64 and ignores the hardware tests, since it does not run inline assembly.
  Of the uses above, only rows 11 and 18 run under Miri.
- No Kani harness covers the hardware CRC32C paths; the harnesses cover the software CRC32C and
  the parsers.
