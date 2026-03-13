// Copyright © 2019 The Rust Fuzz Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Write-side counterpart to [`Unstructured`], for building byte sequences
//! that roundtrip through [`Arbitrary::arbitrary`].

/// A builder for constructing byte sequences that can be parsed back by
/// [`Unstructured`][crate::Unstructured].
///
/// `Structured` mirrors every read operation in `Unstructured` with a
/// corresponding write operation. The key invariant is:
///
/// ```text
/// let bytes = value.to_arbitrary_bytes();
/// let parsed = T::arbitrary(&mut Unstructured::new(&bytes)).unwrap();
/// assert_eq!(value, parsed);
/// ```
///
/// # Size suffixes
///
/// `Unstructured::arbitrary_byte_size` reads collection sizes from the END of
/// the data buffer. `Structured` handles this by deferring size encodings and
/// resolving them in [`into_bytes`](Structured::into_bytes).
#[derive(Debug)]
pub struct Structured {
    /// Content bytes written front-to-back.
    content: Vec<u8>,
    /// Deferred byte-size values. Each entry is the desired `byte_size` that
    /// `arbitrary_byte_size` should return. Stored in order of creation
    /// (first push = outermost = goes at the very end of the output).
    deferred_sizes: Vec<usize>,
    /// Current separator nesting depth.
    separator_depth: u8,
}

impl Structured {
    /// Create a new empty builder.
    pub fn new() -> Self {
        Structured {
            content: Vec::new(),
            deferred_sizes: Vec::new(),
            separator_depth: 0,
        }
    }

    /// Create a new builder with a specific separator nesting depth.
    pub fn new_with_depth(separator_depth: u8) -> Self {
        Structured {
            content: Vec::new(),
            deferred_sizes: Vec::new(),
            separator_depth,
        }
    }

    /// Get the current number of content bytes written.
    pub fn content_len(&self) -> usize {
        self.content.len()
    }

    /// Get the current separator depth.
    pub fn separator_depth(&self) -> u8 {
        self.separator_depth
    }

    /// Write raw bytes (counterpart to `Unstructured::fill_buffer` / `bytes`).
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.content.extend_from_slice(bytes);
    }

    /// Write a separator for the current depth.
    pub fn write_separator(&mut self) {
        let sep = self.separator_magic();
        self.content.extend_from_slice(&sep);
    }

    /// Get the separator magic bytes for the current nesting depth.
    fn separator_magic(&self) -> [u8; 4] {
        crate::unstructured::separator_for_depth(self.separator_depth)
    }

    /// Increment the separator depth for nested collection writing.
    /// Returns the previous depth so the caller can restore it.
    pub fn enter_collection(&mut self) -> u8 {
        let prev = self.separator_depth;
        self.separator_depth += 1;
        prev
    }

    /// Restore the separator depth after writing a nested collection.
    pub fn exit_collection(&mut self, prev_depth: u8) {
        self.separator_depth = prev_depth;
    }

    /// Register a deferred byte-size value. This corresponds to a call to
    /// `Unstructured::arbitrary_byte_size` during reading.
    ///
    /// The size suffix will be appended to the end of the output buffer
    /// when [`into_bytes`](Structured::into_bytes) is called.
    pub fn write_byte_size(&mut self, byte_size: usize) {
        self.deferred_sizes.push(byte_size);
    }

    /// Finalize and return the byte sequence.
    ///
    /// Resolves deferred size suffixes by computing the total buffer length
    /// and encoding each size at the end. Size suffixes are appended in
    /// reverse order of creation (last created goes first in the suffix
    /// region, first created goes at the very end).
    pub fn into_bytes(self) -> Vec<u8> {
        let mut result = self.content;

        if self.deferred_sizes.is_empty() {
            return result;
        }

        // Size suffixes are read in LIFO order: the first deferred_size
        // corresponds to the outermost arbitrary_byte_size call, which reads
        // from the very end of the buffer. So we append them in reverse order.
        //
        // Each suffix's encoding depends on the total buffer length at the
        // time the reader calls arbitrary_byte_size. We iterate to find a
        // stable encoding.
        let reversed: Vec<usize> = self.deferred_sizes.into_iter().rev().collect();

        // Start with 1-byte encoding for each suffix
        let mut suffix_encodings: Vec<Vec<u8>> = reversed.iter().map(|_| vec![0u8]).collect();

        for _ in 0..5 {
            let total_suffix_len: usize = suffix_encodings.iter().map(|s| s.len()).sum();
            // reader_len = total length when the reader starts
            let mut reader_len = result.len() + total_suffix_len;
            let mut new_encodings = Vec::with_capacity(reversed.len());

            for &desired_size in reversed.iter() {
                // The reader has `reader_len` bytes remaining.
                // arbitrary_byte_size determines encoding from reader_len.
                let encoding = encode_byte_size(desired_size, reader_len);
                let enc_len = encoding.len();
                new_encodings.push(encoding);

                // After reading this suffix, reader_len decreases by enc_len
                reader_len -= enc_len;
                // After bytes(desired_size), reader_len decreases by desired_size
                //
                // But we need to handle the case where this is the innermost
                // call — subsequent calls see the reduced buffer.
                reader_len -= desired_size;
            }

            if new_encodings == suffix_encodings {
                // Stable encoding found
                break;
            }
            suffix_encodings = new_encodings;
        }

        for encoding in suffix_encodings {
            result.extend_from_slice(&encoding);
        }

        result
    }
}

impl Default for Structured {
    fn default() -> Self {
        Self::new()
    }
}

/// Encode a desired byte_size value as the suffix bytes that
/// `Unstructured::arbitrary_byte_size` would read to produce that value.
///
/// `reader_data_len` is the total length of `self.data` when the reader
/// calls `arbitrary_byte_size`.
fn encode_byte_size(desired_size: usize, reader_data_len: usize) -> Vec<u8> {
    if reader_data_len == 0 {
        // arbitrary_byte_size returns 0 for empty data, no bytes consumed
        return Vec::new();
    }
    if reader_data_len == 1 {
        // arbitrary_byte_size returns 0 and consumes the 1 byte
        return vec![0];
    }

    // The reader determines encoding type by its current data length:
    //   len <= u8::MAX + 1 (256)    -> 1 byte suffix
    //   len <= u16::MAX + 2 (65537) -> 2 byte suffix
    //   len <= u32::MAX + 4         -> 4 byte suffix
    //   else                        -> 8 byte suffix
    //
    // Then: max_size = len - suffix_bytes
    // Then: int_in_range_impl(0..=max_size, suffix_bytes) -> desired_size
    //
    // int_in_range_impl reads bytes in big-endian order and does:
    //   result = arbitrary_int % (max_size + 1)
    //
    // To reverse: we need arbitrary_int such that arbitrary_int % (max_size + 1) == desired_size.
    // Simplest: arbitrary_int = desired_size (works when desired_size <= max_size).

    let rdl = reader_data_len as u64;
    if rdl <= u8::MAX as u64 + 1 {
        let max_size = reader_data_len - 1;
        let val = (desired_size % (max_size + 1)) as u8;
        vec![val]
    } else if rdl <= u16::MAX as u64 + 2 {
        let max_size = reader_data_len - 2;
        let val = (desired_size % (max_size + 1)) as u16;
        // int_in_range_impl reads big-endian
        val.to_be_bytes().to_vec()
    } else if rdl <= u32::MAX as u64 + 4 {
        let max_size = reader_data_len - 4;
        let val = (desired_size % (max_size + 1)) as u32;
        val.to_be_bytes().to_vec()
    } else {
        let max_size = reader_data_len - 8;
        let val = (desired_size % (max_size + 1)) as u64;
        val.to_be_bytes().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Unstructured;

    #[test]
    fn encode_byte_size_roundtrip() {
        // Build a buffer with a known byte_size suffix, then verify
        // arbitrary_byte_size reads it back correctly.
        for desired in [0, 1, 5, 10, 50, 100] {
            let content_len = desired + 20; // some padding after
            let mut s = Structured::new();
            s.write_bytes(&vec![0xAA; desired]);
            s.write_bytes(&vec![0xBB; content_len - desired]); // "other fields"
            s.write_byte_size(desired);
            let bytes = s.into_bytes();

            let mut u = Unstructured::new(&bytes);
            let read_size = u.arbitrary_byte_size_for_test();
            assert_eq!(read_size, desired, "failed for desired={desired}");
            let data = u.bytes(read_size).unwrap();
            assert_eq!(data, &vec![0xAA; desired][..]);
        }
    }

    #[test]
    fn encode_byte_size_roundtrip_multiple() {
        // Two deferred sizes: outer reads last suffix, inner reads second-to-last.
        let mut s = Structured::new();
        s.write_bytes(&[1, 2, 3]); // outer collection data (3 bytes)
        s.write_byte_size(3);
        s.write_bytes(&[4, 5]); // inner collection data (2 bytes)
        s.write_byte_size(2);
        let bytes = s.into_bytes();

        let mut u = Unstructured::new(&bytes);
        // First arbitrary_byte_size call (outer) should return 3
        let outer_size = u.arbitrary_byte_size_for_test();
        assert_eq!(outer_size, 3);
        let outer_data = u.bytes(outer_size).unwrap();
        assert_eq!(outer_data, &[1, 2, 3]);

        // Second arbitrary_byte_size call (inner) should return 2
        let inner_size = u.arbitrary_byte_size_for_test();
        assert_eq!(inner_size, 2);
        let inner_data = u.bytes(inner_size).unwrap();
        assert_eq!(inner_data, &[4, 5]);
    }
}
