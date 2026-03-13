// Copyright © 2019 The Rust Fuzz Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The [`Dearbitrary`] trait for converting structured data back into byte
//! sequences that roundtrip through [`Arbitrary::arbitrary`].

use crate::{Arbitrary, Structured};

/// The reverse of [`Arbitrary`]: converts a value back into a byte sequence
/// that, when passed to [`Arbitrary::arbitrary`], produces an equal value.
///
/// # Roundtrip property
///
/// For any `value: T` where `T: Dearbitrary + PartialEq`:
///
/// ```text
/// let bytes = value.to_arbitrary_bytes();
/// let parsed = T::arbitrary(&mut Unstructured::new(&bytes)).unwrap();
/// assert_eq!(value, parsed);
/// ```
///
/// # Limitations
///
/// The `for<'a> Arbitrary<'a>` bound naturally excludes borrowed types like
/// `&str` and `&[u8]` that cannot roundtrip (the deserialized value would
/// need to borrow from the input bytes, but the serialized bytes are new).
/// Use owned equivalents like `String` and `Vec<u8>` instead.
pub trait Dearbitrary: for<'a> Arbitrary<'a> {
    /// Write this value's byte representation into the builder.
    ///
    /// The bytes written must be parseable by the corresponding
    /// [`Arbitrary::arbitrary`] implementation to produce an equal value.
    fn write_to(&self, s: &mut Structured);

    /// Write bytes compatible with `Arbitrary::arbitrary_take_rest`.
    ///
    /// Most types serialize identically for both `arbitrary` and
    /// `arbitrary_take_rest`. Types that differ (collections, strings) override
    /// this. Only used internally by collection serialization — users should
    /// not need to call or override this.
    #[doc(hidden)]
    fn write_take_rest_to(&self, s: &mut Structured) {
        self.write_to(s);
    }

    /// Convert this value to a standalone byte sequence.
    ///
    /// This is a convenience wrapper that creates a [`Structured`] builder,
    /// calls [`write_to`](Dearbitrary::write_to), and returns the finalized
    /// bytes.
    fn to_arbitrary_bytes(&self) -> Vec<u8> {
        let mut s = Structured::new();
        self.write_to(&mut s);
        s.into_bytes()
    }
}

// ============================================================================
// Primitive integer implementations
// ============================================================================

macro_rules! impl_dearbitrary_for_integers {
    ( $( $ty:ty; )* ) => {
        $(
            impl Dearbitrary for $ty {
                fn write_to(&self, s: &mut Structured) {
                    s.write_bytes(&self.to_le_bytes());
                }
            }
        )*
    }
}

impl_dearbitrary_for_integers! {
    u8;
    u16;
    u32;
    u64;
    u128;
    i8;
    i16;
    i32;
    i64;
    i128;
}

// usize/isize forward to u64/i64 to match the Arbitrary impl
impl Dearbitrary for usize {
    fn write_to(&self, s: &mut Structured) {
        (*self as u64).write_to(s);
    }
}

impl Dearbitrary for isize {
    fn write_to(&self, s: &mut Structured) {
        (*self as i64).write_to(s);
    }
}

// ============================================================================
// Float implementations
// ============================================================================

impl Dearbitrary for f32 {
    fn write_to(&self, s: &mut Structured) {
        self.to_bits().write_to(s);
    }
}

impl Dearbitrary for f64 {
    fn write_to(&self, s: &mut Structured) {
        self.to_bits().write_to(s);
    }
}

// ============================================================================
// Bool
// ============================================================================

impl Dearbitrary for bool {
    fn write_to(&self, s: &mut Structured) {
        // Arbitrary reads u8, checks & 1 == 1
        let byte: u8 = if *self { 1 } else { 0 };
        s.write_bytes(&[byte]);
    }
}

// ============================================================================
// Char
// ============================================================================

impl Dearbitrary for char {
    fn write_to(&self, s: &mut Structured) {
        // Arbitrary reads u32, then char::from_u32, falls back to replace char
        (*self as u32).write_to(s);
    }
}

// ============================================================================
// Option
// ============================================================================

impl<A: Dearbitrary> Dearbitrary for Option<A> {
    fn write_to(&self, s: &mut Structured) {
        match self {
            Some(val) => {
                true.write_to(s);
                val.write_to(s);
            }
            None => {
                false.write_to(s);
            }
        }
    }
}

// ============================================================================
// Result
// ============================================================================

impl<A: Dearbitrary, B: Dearbitrary> Dearbitrary for Result<A, B> {
    fn write_to(&self, s: &mut Structured) {
        match self {
            Ok(val) => {
                true.write_to(s);
                val.write_to(s);
            }
            Err(val) => {
                false.write_to(s);
                val.write_to(s);
            }
        }
    }
}

// ============================================================================
// Vec
// ============================================================================

/// Build collection data bytes for a slice of Dearbitrary elements.
///
/// For fixed-size elements, concatenates element bytes directly.
/// For variable-size elements, joins with separators at `depth`,
/// with each element serialized into a self-contained sub-builder
/// at `depth + 1` (matching how the reader creates child Unstructured).
fn build_collection_bytes<A: Dearbitrary>(elems: &[A], depth: u8) -> Vec<u8> {
    let (lower, upper) = <A as Arbitrary>::size_hint(0);
    let is_fixed_size = upper == Some(lower) && lower > 0;

    let mut collection = Structured::new_with_depth(depth);

    if is_fixed_size {
        for elem in elems.iter() {
            elem.write_to(&mut collection);
        }
    } else {
        let separator = crate::unstructured::separator_for_depth(depth);
        for (i, elem) in elems.iter().enumerate() {
            if i > 0 {
                collection.write_bytes(&separator);
            }
            // Each element is parsed independently from its chunk via
            // arbitrary_take_rest, so serialize into a self-contained
            // sub-builder at depth+1 (matching new_with_depth in reader).
            let mut elem_builder = Structured::new_with_depth(depth + 1);
            elem.write_take_rest_to(&mut elem_builder);
            let elem_bytes = elem_builder.into_bytes();
            collection.write_bytes(&elem_bytes);
        }
    }

    collection.into_bytes()
}

impl<A: Dearbitrary> Dearbitrary for Vec<A> {
    fn write_to(&self, s: &mut Structured) {
        let collection_bytes = build_collection_bytes(self, s.separator_depth());
        let byte_size = collection_bytes.len();
        s.write_bytes(&collection_bytes);
        s.write_byte_size(byte_size);
    }

    fn write_take_rest_to(&self, s: &mut Structured) {
        // take_rest doesn't call arbitrary_byte_size, so no size suffix
        let collection_bytes = build_collection_bytes(self, s.separator_depth());
        s.write_bytes(&collection_bytes);
    }
}

// ============================================================================
// String
// ============================================================================

impl Dearbitrary for String {
    fn write_to(&self, s: &mut Structured) {
        // String::arbitrary delegates to &str::arbitrary which calls:
        //   arbitrary_len::<u8>() -> arbitrary_byte_size() -> returns len
        //   then reads `len` bytes as UTF-8
        let str_bytes = self.as_bytes();
        s.write_bytes(str_bytes);
        s.write_byte_size(str_bytes.len());
    }

    fn write_take_rest_to(&self, s: &mut Structured) {
        // String::arbitrary_take_rest reads all remaining bytes as UTF-8,
        // no size suffix needed.
        if self.is_empty() {
            // An empty chunk would be skipped by the iterator. Write an
            // invalid UTF-8 byte so the chunk is non-empty but
            // arbitrary_str's from_utf8 sees valid_up_to() == 0 -> "".
            s.write_bytes(&[0xFF]);
        } else {
            s.write_bytes(self.as_bytes());
        }
    }
}

// ============================================================================
// Box, Rc, Arc
// ============================================================================

impl<A: Dearbitrary> Dearbitrary for Box<A> {
    fn write_to(&self, s: &mut Structured) {
        (**self).write_to(s);
    }
}

impl<A: Dearbitrary> Dearbitrary for std::rc::Rc<A> {
    fn write_to(&self, s: &mut Structured) {
        (**self).write_to(s);
    }
}

impl<A: Dearbitrary> Dearbitrary for std::sync::Arc<A> {
    fn write_to(&self, s: &mut Structured) {
        (**self).write_to(s);
    }
}

// ============================================================================
// Tuples
// ============================================================================

macro_rules! impl_dearbitrary_for_tuples {
    () => {};
    ($first:ident $(, $rest:ident)*) => {
        impl<$first: Dearbitrary $(, $rest: Dearbitrary)*> Dearbitrary for ($first, $($rest,)*) {
            #[allow(non_snake_case)]
            fn write_to(&self, s: &mut Structured) {
                let ($first, $($rest,)*) = self;
                $first.write_to(s);
                $($rest.write_to(s);)*
            }
        }
        impl_dearbitrary_for_tuples!($($rest),*);
    };
}

impl_dearbitrary_for_tuples!(A, B, C, D, E, F, G, H, I, J, K, L);

// ============================================================================
// Unit
// ============================================================================

impl Dearbitrary for () {
    fn write_to(&self, _s: &mut Structured) {}
}

// ============================================================================
// Arrays
// ============================================================================

impl<A: Dearbitrary, const N: usize> Dearbitrary for [A; N] {
    fn write_to(&self, s: &mut Structured) {
        for elem in self.iter() {
            elem.write_to(s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Unstructured;

    fn roundtrip<T: Dearbitrary + PartialEq + std::fmt::Debug>(value: &T) {
        let bytes = value.to_arbitrary_bytes();
        let mut u = Unstructured::new(&bytes);
        let parsed = T::arbitrary(&mut u).unwrap();
        assert_eq!(value, &parsed, "roundtrip failed: bytes={:?}", bytes);
    }

    #[test]
    fn roundtrip_integers() {
        roundtrip(&42u8);
        roundtrip(&0u8);
        roundtrip(&255u8);
        roundtrip(&1234u16);
        roundtrip(&0xDEADBEEFu32);
        roundtrip(&0i32);
        roundtrip(&-1i32);
        roundtrip(&i64::MIN);
        roundtrip(&i64::MAX);
        roundtrip(&42usize);
        roundtrip(&42isize);
    }

    #[test]
    fn roundtrip_floats() {
        roundtrip(&3.14f32);
        roundtrip(&0.0f64);
        roundtrip(&f64::INFINITY);
    }

    #[test]
    fn roundtrip_bool() {
        roundtrip(&true);
        roundtrip(&false);
    }

    #[test]
    fn roundtrip_option() {
        roundtrip(&Some(42u32));
        roundtrip(&None::<u32>);
    }

    #[test]
    fn roundtrip_string() {
        roundtrip(&String::from(""));
        roundtrip(&String::from("hello"));
        roundtrip(&String::from("hello world, this is a longer string!"));
    }

    #[test]
    fn roundtrip_vec_u8() {
        roundtrip(&vec![1u8, 2, 3, 4, 5]);
        roundtrip(&Vec::<u8>::new());
        roundtrip(&vec![0u8; 100]);
    }

    #[test]
    fn roundtrip_vec_u32() {
        roundtrip(&vec![1u32, 2, 3]);
        roundtrip(&Vec::<u32>::new());
        roundtrip(&vec![0xDEADBEEFu32]);
    }

    #[test]
    fn roundtrip_vec_string() {
        roundtrip(&vec![String::from("hello"), String::from("world")]);
        roundtrip(&Vec::<String>::new());
        roundtrip(&vec![String::from("")]);
    }

    #[test]
    fn roundtrip_nested_vec() {
        roundtrip(&vec![vec![1u8, 2], vec![3u8, 4]]);
        roundtrip(&vec![vec![vec![1u8]]]);
        roundtrip(&Vec::<Vec<u8>>::new());
    }

    #[test]
    fn roundtrip_tuple() {
        roundtrip(&(1u8, 2u16, 3u32));
        roundtrip(&(true, 42u8));
    }

    #[test]
    fn roundtrip_array() {
        roundtrip(&[1u8, 2, 3]);
        roundtrip(&[0u32; 0]);
        roundtrip(&[42u64; 3]);
    }

    #[test]
    fn roundtrip_box() {
        roundtrip(&Box::new(42u32));
    }
}
