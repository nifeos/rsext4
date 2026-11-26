//! The `extent_tree` target: `rsext4_fuzz::targets::extent_tree` over each input.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| rsext4_fuzz::targets::extent_tree(data));
