//! The `mount_and_walk` target: `rsext4_fuzz::targets::mount_and_walk` over each input.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| rsext4_fuzz::targets::mount_and_walk(data));
