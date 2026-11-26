//! The `journal_replay` target: `rsext4_fuzz::targets::journal_replay` over each input.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| rsext4_fuzz::targets::journal_replay(data));
