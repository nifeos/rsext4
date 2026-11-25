//! The switch that turns on the hardware CRC32C path.
//!
//! The switch is process-wide, so this test has a test binary of its own: turning it on in the
//! library's own tests would send every checksum those tests compute through the hardware path,
//! on whatever CPU runs them. This binary computes no checksum after the call.

use rsext4::crc32c::{enable_hardware_crc32c, hardware_crc32c_enabled};

#[test]
fn enabling_hardware_crc32c_is_reported() {
    assert!(
        !hardware_crc32c_enabled(),
        "the table-driven path is the default"
    );
    unsafe {
        // SAFETY: nothing in this process computes a checksum after the call, so no CPU runs
        // the hardware instructions it allows.
        enable_hardware_crc32c();
    }
    assert!(hardware_crc32c_enabled());
}
