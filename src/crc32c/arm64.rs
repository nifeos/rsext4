// In `core::arch::aarch64`, the intrinsics with the `c` suffix implement the
// Castagnoli polynomial used by CRC32C.

#[cfg(target_arch = "aarch64")]
#[allow(dead_code)]
use core::arch::aarch64::{__crc32cb, __crc32cd, __crc32ch, __crc32cw};
/// Computes CRC32C with ARMv8 hardware instructions.
///
/// The implementation handles 64/32/16/8-bit chunks to maximize throughput.
///
/// # Safety
///
/// This function uses the `crc` target feature and must only run on CPUs that
/// support the CRC instruction set.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "crc")] // Tell the compiler the CRC feature is available here.
#[inline]
#[allow(dead_code)]
pub unsafe fn crc32c_hardware(mut crc: u32, data: &[u8]) -> u32 {
    // This helper only performs the raw CRC update step. Callers remain
    // responsible for any ext4-specific init/finalize inversion.
    let (chunks, mut tail) = data.as_chunks::<8>();
    for chunk in chunks {
        crc = __crc32cd(crc, u64::from_le_bytes(*chunk));
    }
    if let Some((word, rest)) = tail.split_first_chunk::<4>() {
        crc = __crc32cw(crc, u32::from_le_bytes(*word));
        tail = rest;
    }
    if let Some((half, rest)) = tail.split_first_chunk::<2>() {
        crc = __crc32ch(crc, u16::from_le_bytes(*half));
        tail = rest;
    }
    if let Some(&byte) = tail.first() {
        crc = __crc32cb(crc, byte);
    }
    crc
}
