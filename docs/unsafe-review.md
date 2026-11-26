# Review of the unsafe code

This is a review of every use of `unsafe` in the crate. Line numbers are those of the tree that
carries this file. It is the input to a maintainer's own review by hand, not a replacement for
one: the section "What this review could not verify" lists what was reasoned about but not run or
proved.

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
(`src/crc32c/`) or in its tests. The crate root denies `unsafe_code` and only the `crc32c` module
allows it, so the compiler refuses unsafe code anywhere else in the library. `fuzz/` and
`examples/` hold none, and there is no `unsafe impl`, `unsafe trait`, `static mut`,
`#[no_mangle]` or `#[repr(packed)]` anywhere. `src/hashtree/lookup.rs` also forbids unsafe code
itself. The `fuzz_target!` macro of `libfuzzer-sys` expands to an `unsafe extern "C"` entry point;
that is the dependency's code, not reviewed here.

No code reinterprets bytes as a struct or dereferences a raw pointer. A search for `transmute`,
`from_raw_parts`, `read_unaligned`, `write_unaligned`, `as *const`, `as *mut`, `&*`, `ptr::`,
`addr_of`, `MaybeUninit`, `zeroed`, `align_to`, `get_unchecked` and `set_len` finds only address
arithmetic on `as_ptr() as usize` in the Kani harnesses of `src/entries/verification.rs` and in
`tests/hardware_crc32c_random.rs`, which records the alignment of each slice it checks, and
pointer comparisons in a test of `src/jbd2/jbd2.rs`; none of them dereferences a pointer. Both
hardware CRC32C helpers read their input through `as_chunks` and `from_le_bytes`, so they read the
bytes in memory order on either byte order. The `#[repr(C)]` on-disk structs are decoded and
encoded field by field through `from_le_bytes` and `to_le_bytes`.

## The group descriptor casts are gone

Group descriptors were once read by casting a byte offset in the table to `&Ext4GroupDesc`.
`src/blockgroup_description/table.rs` now takes each record's byte range with checked arithmetic
(`record_range`), decodes it with `Ext4GroupDesc::decode_checked` at the descriptor size the
superblock gives, and writes it back with `encode_checked`. `get_desc_mut` no longer exists, and
the crate has no pointer cast left that could produce a reference to a descriptor.

## Each use

The table lists every use that remains and why it stays: each is a CPU instruction safe code
cannot reach, a contract only the caller can establish, or a foreign function.

| # | location | what it relies on | why it stays |
|---|---|---|---|
| 1 | `src/crc32c/x86_64.rs:15` `unsafe fn crc32c_hardware` | `# Safety`: every CPU that may execute the call implements `sse4.2`. The function reads `data` only through safe slice operations, so the CPU feature is its whole precondition. | the CPU feature is a fact about the machine that only the caller can establish |
| 2 | `src/crc32c/x86_64.rs:20` `asm!` `crc32 r64, r64` | The caller's `sse4.2` guarantee. Both operands are registers, so `nomem` and `nostack` hold, and CRC32 changes no flags (the Intel manual for CRC32 lists "Flags Affected: None"), so `preserves_flags` holds. | inline assembly is always unsafe; the module uses it so that the code needs no single instruction, multiple data (SIMD) register state, as its header says |
| 3 | `src/crc32c/x86_64.rs:34` `asm!` `crc32 r32, r8` | As row 2. `reg_byte` on x86_64 never picks `ah` to `dh`, which cannot be encoded with the REX prefix the instruction may need. | as row 2 |
| 4 | `src/crc32c/arm64.rs:19` `unsafe fn crc32c_hardware`, `#[target_feature(enable = "crc")]` | `# Safety`: it runs only on CPUs with the CRC instructions. Its body is safe code: inside the function the CRC32C intrinsics are safe to call, and the input is read through `as_chunks`, `split_first_chunk` and `from_le_bytes`. | as row 1; a safe `#[target_feature]` function would still be unsafe to call from `crc32c_append`, and `unsafe fn` keeps the `# Safety` section next to the contract |
| 5 | `src/crc32c/crc32c.rs:126` `unsafe fn enable_hardware_crc32c` | `# Safety`: every CPU that may run the crate's code from the call on implements the instructions. The function only stores a flag. `Relaxed` is enough: a CPU that has not yet seen the store keeps the table-driven path, which needs nothing. | it is where the environment hands that guarantee to the crate |
| 6 | `src/crc32c/crc32c.rs:143` call of `crc32c_hardware` in `crc32c_append` | The flag is set only by row 5, whose caller guarantees the instructions on every CPU, which is the contract of rows 1 and 4. | calling an `unsafe fn` |
| 7 | `src/crc32c/crc32c.rs:316` aarch64 test, call of `crc32c_hardware` | The test returns early unless `is_aarch64_feature_detected!("crc")`. On Linux and Android std reads `HWCAP_CRC32` from `AT_HWCAP`, which the kernel sets from the value it sanitizes across all CPUs and refuses to bring up a later CPU without; on macOS every CPU is version 8.1 of the Arm architecture or later. The test is built only on those systems. | the test exercises row 4 directly |
| 8 | `src/crc32c/crc32c.rs:362` `unsafe extern "C"` block | The declarations must match the C prototypes: `int sched_getcpu(void)` and `int sched_setaffinity(pid_t, size_t, const cpu_set_t *)`, with `int` and `pid_t` 32-bit signed and `size_t` equal to `usize` on x86_64 Linux. They do, and the 16-word `CpuSet` is the 1024-bit `cpu_set_t` of glibc and musl. `sched_getcpu` takes no arguments and only reads state, so it is declared `safe fn` and its call needs no `unsafe`. | edition 2024 requires every `extern` block to be `unsafe` |
| 9 | `src/crc32c/crc32c.rs:393` `sched_setaffinity(0, size_of::<CpuSet>(), &mask)` | `mask` is a live `CpuSet` of the size passed, which the kernel only reads; pid 0 is the calling thread. A bit past the set is refused before the call. | it takes a raw pointer and a length that must agree |
| 10 | `src/crc32c/crc32c.rs:413` x86_64 test, call of `crc32c_hardware` | The thread is pinned to one CPU, and `cpuid` leaf 1, `ecx` bit 20 read after pinning reported `sse4.2`. If the thread had moved between `sched_getcpu` and `sched_setaffinity`, Linux migrates it to the one allowed CPU before the call returns. The comment states the remaining assumption: no CPU hot-unplug, cpuset change or outside affinity change during the test. Breaking it would raise an illegal instruction, not corrupt memory. | the test exercises row 1 directly |
| 11 | `tests/hardware_crc32c_switch.rs:15` call of `enable_hardware_crc32c` | The binary holds this one test, and nothing in it computes a checksum after the call, so no CPU runs the instructions the call allows. It stops holding if a test that computes a checksum is added to this binary, as the file's header says. | the test checks that row 5 sets the flag |
| 12 | `tests/hardware_crc32c_random.rs:107` call of `enable_hardware_crc32c` | As row 7: the test returns early unless std reports `crc`, and is built only on Linux, Android and macOS on aarch64. | the test checks the hardware path through the public switch on random inputs |

Of the 12 uses, six are in the library and six in tests. Row 12 is in a test only this repository
carries; the other eleven are in code that upstream has or that this fork offers upstream.

## Lints

`Cargo.toml` sets each lint that governs the unsafe code explicitly:

- `src/lib.rs` denies `unsafe_code` at the crate root and allows it only on the `crc32c` module.
- `unsafe_op_in_unsafe_fn` is `deny`, so each unsafe operation in an `unsafe fn` needs its own
  `unsafe` block. Both `unsafe fn` bodies comply; the aarch64 one has no unsafe operation.
- `clippy::missing_safety_doc` and `clippy::undocumented_unsafe_blocks` are `deny`. They report
  nothing for `--all-targets` on x86_64 Linux and aarch64 Linux and for `--lib` on
  `aarch64-unknown-none-softfloat`, `riscv64imac-unknown-none-elf` and `x86_64-unknown-none`. The
  lint accepts the `// SAFETY:` comment as the first line inside the block, which is the placement
  upstream uses. It does not check `unsafe extern` blocks; row 8 carries its comment anyway.
- `clippy::multiple_unsafe_ops_per_block` is not enabled; no block holds more than one unsafe
  operation.

## What this review could not verify

- The aarch64 hardware path (rows 4, 7 and 12) runs in the `hardware CRC32C (aarch64)` job of
  `.github/workflows/ci.yml` on a hosted arm64 runner, a little-endian Linux host at EL0. Nothing
  runs it big-endian or at EL1; for big-endian, this review read the generated code
  of `aarch64_be-unknown-none-softfloat` instead, and it byte-swaps each load.
- Rows 7, 10 and 12 rely on Linux behavior (the kernel sanitizes `HWCAP_CRC32` across CPUs and
  refuses a late CPU without it; `sched_setaffinity` migrates the thread before it returns) that
  this review takes from the kernel's documented behavior and did not test.
- Miri runs weekly on x86_64 and ignores the hardware tests, since it does not run inline assembly.
  Of the uses above, only rows 5 and 11 run under Miri.
- No Kani harness covers the hardware CRC32C paths; the harnesses cover the software CRC32C and
  the parsers.
