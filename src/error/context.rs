//! Structured context carried alongside an ext4 domain error.

use core::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorContext {
    BlockRange { block_id: u32, max_blocks: u64 },
    BlockSize { size: usize, expected: usize },
    BufferSize { provided: usize, required: usize },
    Alignment { offset: u64, alignment: u32 },
    Operation { op: &'static str },
    Feature { set: FeatureSet, bits: u32 },
    Capability { name: &'static str },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureSet {
    Incompatible,
    ReadOnlyCompatible,
}

impl fmt::Display for ErrorContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorContext::BlockRange {
                block_id,
                max_blocks,
            } => write!(f, "block_id={block_id}, max_blocks={max_blocks}"),
            ErrorContext::BlockSize { size, expected } => {
                write!(f, "size={size}, expected={expected}")
            }
            ErrorContext::BufferSize { provided, required } => {
                write!(f, "provided={provided}, required={required}")
            }
            ErrorContext::Alignment { offset, alignment } => {
                write!(f, "offset={offset}, alignment={alignment}")
            }
            ErrorContext::Operation { op } => write!(f, "op={op}"),
            ErrorContext::Feature { set, bits } => write!(f, "set={set:?}, bits={bits:#x}"),
            ErrorContext::Capability { name } => write!(f, "capability={name}"),
        }
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use alloc::string::ToString;

    use super::*;

    #[test]
    fn display_prints_every_context_field() {
        let cases = [
            (
                ErrorContext::BlockRange {
                    block_id: 7,
                    max_blocks: 64,
                },
                "block_id=7, max_blocks=64",
            ),
            (
                ErrorContext::BlockSize {
                    size: 512,
                    expected: 4096,
                },
                "size=512, expected=4096",
            ),
            (
                ErrorContext::BufferSize {
                    provided: 4,
                    required: 8,
                },
                "provided=4, required=8",
            ),
            (
                ErrorContext::Alignment {
                    offset: 513,
                    alignment: 512,
                },
                "offset=513, alignment=512",
            ),
            (
                ErrorContext::Operation { op: "dir:lookup" },
                "op=dir:lookup",
            ),
            (
                ErrorContext::Capability { name: "flush" },
                "capability=flush",
            ),
        ];
        for (context, expected) in cases {
            assert_eq!(context.to_string(), expected);
        }
    }

    #[test]
    fn display_prints_feature_bits_in_hex_with_the_feature_set() {
        let incompatible = ErrorContext::Feature {
            set: FeatureSet::Incompatible,
            bits: 0x8000_0000,
        };
        let read_only = ErrorContext::Feature {
            set: FeatureSet::ReadOnlyCompatible,
            bits: 0x400,
        };

        assert_eq!(
            incompatible.to_string(),
            "set=Incompatible, bits=0x80000000"
        );
        assert_eq!(read_only.to_string(), "set=ReadOnlyCompatible, bits=0x400");
    }
}
