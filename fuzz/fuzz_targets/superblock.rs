//! The `superblock` target: `rsext4_fuzz::targets::superblock` over each input.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| rsext4_fuzz::targets::superblock(data));
