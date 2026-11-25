//! Error types for bitmap operations.

/// Errors returned by bitmap mutation helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitmapError {
    /// The requested bit index is outside the bitmap range.
    IndexOutOfRange,
    /// The target bit is already allocated.
    AlreadyAllocated,
    /// The target bit is already free.
    AlreadyFree,
}

impl core::fmt::Display for BitmapError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BitmapError::IndexOutOfRange => write!(f, "bitmap index out of range"),
            BitmapError::AlreadyAllocated => write!(f, "bitmap entry is already allocated"),
            BitmapError::AlreadyFree => write!(f, "bitmap entry is already free"),
        }
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use alloc::string::ToString;

    use super::*;

    #[test]
    fn display_names_each_bitmap_failure() {
        assert_eq!(
            BitmapError::IndexOutOfRange.to_string(),
            "bitmap index out of range"
        );
        assert_eq!(
            BitmapError::AlreadyAllocated.to_string(),
            "bitmap entry is already allocated"
        );
        assert_eq!(
            BitmapError::AlreadyFree.to_string(),
            "bitmap entry is already free"
        );
    }
}
