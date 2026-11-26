//! The aarch64 hardware CRC32C path, checked against the table-driven path on random inputs.
//!
//! The switch is process-wide, so this test has a binary of its own: it computes every expected
//! checksum on the table-driven path first, then turns the hardware path on and computes each one
//! again through the same public function.
//!
//! The inputs are random slices of a random buffer, at every alignment and at lengths of every
//! remainder modulo eight, with random starting accumulators. The seed is printed, and setting
//! `RSEXT4_CRC32C_SEED` repeats a run. Setting `RSEXT4_REQUIRE_HARDWARE_CRC32C` makes a host
//! whose CPU lacks the `crc` feature fail instead of skip, which is how CI shows the path ran.
//!
//! The safety argument holds where std's `crc` detection describes every CPU, which this test can
//! show only for Linux, Android and macOS, so other hosts do not build it.

#![cfg(all(
    target_arch = "aarch64",
    any(target_os = "linux", target_os = "android", target_os = "macos")
))]

use rsext4::crc32c::{crc32c_append, enable_hardware_crc32c, hardware_crc32c_enabled};

const BUFFER_LEN: usize = 16 * 1024;
const CASES: usize = 20_000;

/// SplitMix64: enough to spread inputs, with no dependency.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

fn seed() -> u64 {
    if let Ok(text) = std::env::var("RSEXT4_CRC32C_SEED") {
        let text = text.trim_start_matches("0x");
        return u64::from_str_radix(text, 16).expect("RSEXT4_CRC32C_SEED is hexadecimal");
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    now.as_secs() ^ u64::from(now.subsec_nanos()).rotate_left(32)
}

#[test]
fn hardware_crc32c_matches_table_on_random_inputs() {
    if !std::arch::is_aarch64_feature_detected!("crc") {
        assert!(
            std::env::var_os("RSEXT4_REQUIRE_HARDWARE_CRC32C").is_none(),
            "RSEXT4_REQUIRE_HARDWARE_CRC32C is set, but this CPU lacks the CRC32 extension"
        );
        println!("RSEXT4_AARCH64_HARDWARE_CRC32C_TEST_SKIPPED reason=no-crc");
        return;
    }

    let seed = seed();
    println!("RSEXT4_CRC32C_SEED={seed:#x}");
    let mut rng = SplitMix64(seed);

    let mut buffer = vec![0u8; BUFFER_LEN];
    for chunk in buffer.chunks_mut(8) {
        let bytes = rng.next().to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }

    // Each case is (start, length, accumulator). Half the lengths are short, so the tails and the
    // slices with no whole 8-byte chunk are well covered.
    let cases: Vec<(usize, usize, u32)> = (0..CASES)
        .map(|_| {
            let start = rng.below(BUFFER_LEN);
            let room = BUFFER_LEN - start;
            let len = if rng.next() & 1 == 0 {
                rng.below(room.min(64) + 1)
            } else {
                rng.below(room + 1)
            };
            (start, len, rng.next() as u32)
        })
        .collect();

    let mut alignments = [false; 8];
    let mut remainders = [false; 8];
    for &(start, len, _) in &cases {
        alignments[(buffer[start..].as_ptr() as usize) % 8] = true;
        remainders[len % 8] = true;
    }
    assert!(alignments.iter().all(|&hit| hit), "seed={seed:#x}");
    assert!(remainders.iter().all(|&hit| hit), "seed={seed:#x}");

    assert!(
        !hardware_crc32c_enabled(),
        "the table-driven path is the default"
    );
    let expected: Vec<u32> = cases
        .iter()
        .map(|&(start, len, crc)| crc32c_append(crc, &buffer[start..start + len]))
        .collect();

    unsafe {
        // SAFETY: the test returned early above unless std reported `crc`. On Linux and Android
        // that is `HWCAP_CRC32`, which the kernel sets only when every CPU has the extension and
        // refuses to bring up a later CPU without it; on macOS every CPU is ARMv8.1 or later,
        // where the extension is mandatory. So every CPU that may run this process has it.
        enable_hardware_crc32c();
    }
    assert!(hardware_crc32c_enabled());

    for (&(start, len, crc), &want) in cases.iter().zip(&expected) {
        let data = &buffer[start..start + len];
        assert_eq!(
            crc32c_append(crc, data),
            want,
            "seed={seed:#x} start={start} misalignment={} length={len} crc={crc:#x}",
            data.as_ptr() as usize % 8
        );
    }
}
