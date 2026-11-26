#[cfg(target_arch = "aarch64")]
#[allow(dead_code)]
use core::arch::asm;
#[cfg(target_arch = "aarch64")]
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_arch = "aarch64")]
static CRC32_HW_CHECKED: AtomicBool = AtomicBool::new(false);
#[cfg(target_arch = "aarch64")]
static CRC32_HW_SUPPORTED: AtomicBool = AtomicBool::new(false);

/// Returns whether the first CPU to call this supports hardware CRC32C
/// instructions.
///
/// The result is computed once on first call and cached for all subsequent calls.
/// It describes only that first CPU, so it does not establish the precondition
/// of [`crc32c_hardware`] for a later call that may run on another CPU.
/// rsext4 no longer uses it; see `enable_hardware_crc32c`.
///
/// Safety: `CRC32_HW_CHECKED` (Acquire/Release) provides happens-before ordering;
/// `CRC32_HW_SUPPORTED` uses Relaxed because it is always accessed after
/// the Acquire load on `CRC32_HW_CHECKED` has observed the init-complete store.
#[cfg(target_arch = "aarch64")]
#[deprecated(note = "describes only the first CPU to call it; call \
                     `rsext4::crc32c::enable_hardware_crc32c` from code that can guarantee the \
                     feature on every CPU")]
#[inline]
pub fn is_hardware_crc32_supported() -> bool {
    if CRC32_HW_CHECKED.load(Ordering::Acquire) {
        return CRC32_HW_SUPPORTED.load(Ordering::Relaxed);
    }
    let supported = has_hardware_crc32();
    CRC32_HW_SUPPORTED.store(supported, Ordering::Relaxed);
    CRC32_HW_CHECKED.store(true, Ordering::Release);
    supported
}

// In `core::arch::aarch64`, the intrinsics with the `c` suffix implement the
// Castagnoli polynomial used by CRC32C.

#[cfg(target_arch = "aarch64")]
#[allow(dead_code)]
use core::arch::aarch64::{__crc32cb, __crc32cd, __crc32ch, __crc32cw};
/// Returns whether the current CPU advertises CRC32/CRC32C instructions.
///
/// This reads `ID_AA64ISAR0_EL1[19:16]` on the CPU that executes it, so the
/// answer says nothing about other CPUs. rsext4 does not call it on the
/// checksum path.
#[cfg(target_arch = "aarch64")]
#[inline]
#[allow(dead_code)]
pub fn has_hardware_crc32() -> bool {
    let mut reg_val: u64;
    unsafe {
        // SAFETY: `mrs` only copies the read-only ID register into a
        // general-purpose register and touches no memory. At EL1 the read is
        // architectural. At EL0 it traps, and the instruction is only valid
        // there because the kernel emulates the ID registers for user space
        // (Linux does so when it sets `HWCAP_CPUID`); a host that does not
        // emulate them delivers the trap as an illegal instruction.
        // mrs: Move from System Register to general purpose register
        asm!("mrs {}, ID_AA64ISAR0_EL1", out(reg) reg_val);
    }

    // Bits [19:16] encode CRC32 feature support.
    let crc_field = (reg_val >> 16) & 0xF;

    // `>= 1` means CRC32 and CRC32C instructions are present.
    crc_field >= 1
}

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
