//! The `group_descriptor_table` target: `rsext4_fuzz::targets::group_descriptor_table` over each input.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| rsext4_fuzz::targets::group_descriptor_table(data));
