// In `core::arch::aarch64`, the intrinsics with the `c` suffix implement the
// Castagnoli polynomial used by CRC32C.
//
// This path exists only on little-endian aarch64. Its loads read the input in
// native byte order, and the CRC32C instructions take the low byte of each
// operand first, which is the byte at the lowest address only on a
// little-endian target. On big-endian aarch64, rsext4 has only the
// table-driven path.

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[allow(dead_code)]
use core::arch::aarch64::{__crc32cb, __crc32cd, __crc32ch, __crc32cw};
/// Computes CRC32C with ARMv8 hardware instructions.
///
/// The implementation handles 64/32/16/8-bit chunks to maximize throughput.
/// It is built only on little-endian aarch64.
///
/// # Safety
///
/// This function uses the `crc` target feature and must only run on CPUs that
/// support the CRC instruction set.
#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[target_feature(enable = "crc")] // Tell the compiler the CRC feature is available here.
#[inline]
#[allow(dead_code)]
pub unsafe fn crc32c_hardware(mut crc: u32, data: &[u8]) -> u32 {
    // This helper only performs the raw CRC update step. Callers remain
    // responsible for any ext4-specific init/finalize inversion.
    let mut p = data.as_ptr();
    let mut len = data.len();

    // Every read below is in bounds because `len` counts the bytes left
    // between `p` and the end of `data`, and each step reads at most `len`
    // bytes before advancing `p` and lowering `len` by the same amount. Each
    // load reads in native byte order, which is memory order here because the
    // module is built only on little-endian targets.

    // 1. Consume the unaligned prefix so the hot loop can use aligned 64-bit
    //    loads, which a target with strict alignment needs to read a word at
    //    once.
    while len > 0 && !(p as usize).is_multiple_of(8) {
        // SAFETY: `len > 0`, so `p` points at a byte of `data`.
        let byte = unsafe { p.read() };
        crc = __crc32cb(crc, byte);
        // SAFETY: `p` points at a byte of `data`, so one past it is within
        // `data` or one past its end.
        p = unsafe { p.add(1) };
        len -= 1;
    }

    // 2. Main loop: process aligned 64-bit chunks.
    while len >= 8 {
        // SAFETY: the prefix loop left `p` 8-byte aligned and this loop
        // advances it in steps of 8, so the read is aligned; `len >= 8` keeps
        // all eight bytes in `data`.
        let val = unsafe { p.cast::<u64>().read() };
        crc = __crc32cd(crc, val);
        // SAFETY: `len >= 8`, so `p + 8` is within `data` or one past its end.
        p = unsafe { p.add(8) };
        len -= 8;
    }

    // 3. Finish the tail with progressively smaller chunk sizes.
    if len >= 4 {
        // SAFETY: `p` is 8-byte aligned here, so also 4-byte aligned;
        // `len >= 4` keeps the four bytes in `data`.
        let val = unsafe { p.cast::<u32>().read() };
        crc = __crc32cw(crc, val);
        // SAFETY: `len >= 4`, so `p + 4` is within `data` or one past its end.
        p = unsafe { p.add(4) };
        len -= 4;
    }

    if len >= 2 {
        // SAFETY: `p` advanced by 0 or 4 from an 8-byte boundary, so it is
        // 2-byte aligned; `len >= 2` keeps the two bytes in `data`.
        let val = unsafe { p.cast::<u16>().read() };
        crc = __crc32ch(crc, val);
        // SAFETY: `len >= 2`, so `p + 2` is within `data` or one past its end.
        p = unsafe { p.add(2) };
        len -= 2;
    }

    if len > 0 {
        // SAFETY: `len > 0`, so `p` points at the last byte of `data`.
        let byte = unsafe { p.read() };
        crc = __crc32cb(crc, byte);
    }

    crc
}
