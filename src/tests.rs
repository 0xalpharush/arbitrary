use {
    super::{Arbitrary, Result, Unstructured},
    std::{collections::HashSet, fmt::Debug, hash::Hash, rc::Rc, sync::Arc},
};

/// Assert that the given expected values are all generated.
///
/// Exhaustively enumerates all buffers up to length 10 containing the
/// following bytes: `0x00`, `0x01`, `0x61` (aka ASCII 'a'), and `0xff`
fn assert_generates<T>(expected_values: impl IntoIterator<Item = T>)
where
    T: Clone + Debug + Hash + Eq + for<'a> Arbitrary<'a>,
{
    let expected_values: HashSet<_> = expected_values.into_iter().collect();
    let mut arbitrary_expected = expected_values.clone();
    let mut arbitrary_take_rest_expected = expected_values;

    let bytes = [0, 1, b'a', 0xff];
    let max_len = 10;

    let mut buf = Vec::with_capacity(max_len);

    let mut g = exhaustigen::Gen::new();
    while !g.done() {
        let len = g.gen(max_len);

        buf.clear();
        buf.extend(
            std::iter::repeat_with(|| {
                let index = g.gen(bytes.len() - 1);
                bytes[index]
            })
            .take(len),
        );

        let mut u = Unstructured::new(&buf);
        let val = T::arbitrary(&mut u).unwrap();
        arbitrary_expected.remove(&val);

        let u = Unstructured::new(&buf);
        let val = T::arbitrary_take_rest(u).unwrap();
        arbitrary_take_rest_expected.remove(&val);

        if arbitrary_expected.is_empty() && arbitrary_take_rest_expected.is_empty() {
            return;
        }
    }

    panic!(
        "failed to generate all expected values!\n\n\
         T::arbitrary did not generate: {arbitrary_expected:#?}\n\n\
         T::arbitrary_take_rest did not generate {arbitrary_take_rest_expected:#?}"
    )
}

/// Generates an arbitrary `T`, and checks that the result is consistent with the
/// `size_hint()` reported by `T`.
fn checked_arbitrary<'a, T: Arbitrary<'a>>(u: &mut Unstructured<'a>) -> Result<T> {
    let (min, max) = T::size_hint(0);

    let len_before = u.len();
    let result = T::arbitrary(u);

    let consumed = len_before - u.len();

    if let Some(max) = max {
        assert!(
            consumed <= max,
            "incorrect maximum size: indicated {}, actually consumed {}",
            max,
            consumed
        );
    }

    if result.is_ok() {
        assert!(
            consumed >= min,
            "incorrect minimum size: indicated {}, actually consumed {}",
            min,
            consumed
        );
    }

    result
}

/// Like `checked_arbitrary()`, but calls `arbitrary_take_rest()` instead of `arbitrary()`.
fn checked_arbitrary_take_rest<'a, T: Arbitrary<'a>>(u: Unstructured<'a>) -> Result<T> {
    let (min, _) = T::size_hint(0);

    let len_before = u.len();
    let result = T::arbitrary_take_rest(u);

    if result.is_ok() {
        assert!(
            len_before >= min,
            "incorrect minimum size: indicated {}, worked with {}",
            min,
            len_before
        );
    }

    result
}

#[test]
fn finite_buffer_fill_buffer() {
    let x = [1, 2, 3, 4];
    let mut rb = Unstructured::new(&x);
    let mut z = [0; 2];
    rb.fill_buffer(&mut z).unwrap();
    assert_eq!(z, [1, 2]);
    rb.fill_buffer(&mut z).unwrap();
    assert_eq!(z, [3, 4]);
    rb.fill_buffer(&mut z).unwrap();
    assert_eq!(z, [0, 0]);
}

#[test]
fn arbitrary_for_integers() {
    let x = [1, 2, 3, 4];
    let mut buf = Unstructured::new(&x);
    let expected = 1 | (2 << 8) | (3 << 16) | (4 << 24);
    let actual = checked_arbitrary::<i32>(&mut buf).unwrap();
    assert_eq!(expected, actual);

    assert_generates([
        i32::from_ne_bytes([0, 0, 0, 0]),
        i32::from_ne_bytes([0, 0, 0, 1]),
        i32::from_ne_bytes([0, 0, 1, 0]),
        i32::from_ne_bytes([0, 1, 0, 0]),
        i32::from_ne_bytes([1, 0, 0, 0]),
        i32::from_ne_bytes([1, 1, 1, 1]),
        i32::from_ne_bytes([0xff, 0xff, 0xff, 0xff]),
    ]);
}

#[test]
fn arbitrary_for_bytes() {
    let x = [1, 2, 3, 4, 4];
    let mut buf = Unstructured::new(&x);
    let expected = &[1, 2, 3, 4];
    let actual = checked_arbitrary::<&[u8]>(&mut buf).unwrap();
    assert_eq!(expected, actual);
}

#[test]
fn arbitrary_take_rest_for_bytes() {
    let x = [1, 2, 3, 4];
    let buf = Unstructured::new(&x);
    let expected = &[1, 2, 3, 4];
    let actual = checked_arbitrary_take_rest::<&[u8]>(buf).unwrap();
    assert_eq!(expected, actual);
}

#[test]
fn arbitrary_for_vec_u8() {
    assert_generates::<Vec<u8>>([
        vec![],
        vec![0],
        vec![1],
        vec![0, 0],
        vec![0, 1],
        vec![1, 0],
        vec![1, 1],
        vec![0, 0, 0],
        vec![0, 0, 1],
        vec![0, 1, 0],
        vec![0, 1, 1],
        vec![1, 0, 0],
        vec![1, 0, 1],
        vec![1, 1, 0],
        vec![1, 1, 1],
    ]);
}

#[test]
fn arbitrary_for_vec_vec_u8() {
    // Vec<Vec<u8>>: outer vec uses separator-based splitting (Vec<u8> is
    // variable-size), inner vec uses fixed-size chunking (u8 is 1 byte).
    // The depth-0 separator is [0xDE, 0xAD, 0xBE, 0xEF].

    // Empty input -> empty outer vec
    let result = Vec::<Vec<u8>>::arbitrary(&mut Unstructured::new(&[])).unwrap();
    assert_eq!(result, Vec::<Vec<u8>>::new());

    // No separator in collection data -> single inner vec
    // Input: [0x01, 0x02, <size_suffix=3>]
    // arbitrary_byte_size reads 1 byte from end (len=3, u8 encoding), max_size=2
    // int_in_range(0..=2, [3]) -> 3 % 3 = 0... let me just use take_rest
    let result = Vec::<Vec<u8>>::arbitrary_take_rest(Unstructured::new(&[0x01, 0x02])).unwrap();
    assert_eq!(result, vec![vec![0x01, 0x02]]);

    // Separator splits into two inner vecs
    // [0x01, 0x02, SEPARATOR, 0x03, 0x04]
    let mut input = vec![0x01, 0x02, 0xDE, 0xAD, 0xBE, 0xEF, 0x03, 0x04];
    let result = Vec::<Vec<u8>>::arbitrary_take_rest(Unstructured::new(&input)).unwrap();
    assert_eq!(result, vec![vec![0x01, 0x02], vec![0x03, 0x04]]);

    // Three inner vecs with two separators
    input = vec![
        0x0A, 0xDE, 0xAD, 0xBE, 0xEF, 0x0B, 0x0C, 0xDE, 0xAD, 0xBE, 0xEF, 0x0D,
    ];
    let result = Vec::<Vec<u8>>::arbitrary_take_rest(Unstructured::new(&input)).unwrap();
    assert_eq!(result, vec![vec![0x0A], vec![0x0B, 0x0C], vec![0x0D]]);

    // Separator at the start -> first chunk is empty (skipped), second has data
    input = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01];
    let result = Vec::<Vec<u8>>::arbitrary_take_rest(Unstructured::new(&input)).unwrap();
    assert_eq!(result, vec![vec![0x01]]);
}

#[test]
fn arbitrary_for_vec_vec_vec_u8() {
    // Vec<Vec<Vec<u8>>>: depth-0 separator splits outer, depth-1 separator
    // splits middle, inner Vec<u8> uses fixed-size chunking.
    // Depth-0 sep: [0xDE, 0xAD, 0xBE, 0xEF]
    // Depth-1 sep: [0xDF, 0xAC, 0xBF, 0xEE]

    // Single outer element containing two middle vecs split by depth-1 separator
    let input = vec![0x01, 0xDF, 0xAC, 0xBF, 0xEE, 0x02];
    let result = Vec::<Vec<Vec<u8>>>::arbitrary_take_rest(Unstructured::new(&input)).unwrap();
    assert_eq!(result, vec![vec![vec![0x01], vec![0x02]]]);

    // Two outer elements, each with one middle vec
    let input = vec![0x01, 0xDE, 0xAD, 0xBE, 0xEF, 0x02];
    let result = Vec::<Vec<Vec<u8>>>::arbitrary_take_rest(Unstructured::new(&input)).unwrap();
    assert_eq!(result, vec![vec![vec![0x01]], vec![vec![0x02]]]);

    // Empty
    let result = Vec::<Vec<Vec<u8>>>::arbitrary_take_rest(Unstructured::new(&[])).unwrap();
    assert_eq!(result, Vec::<Vec<Vec<u8>>>::new());
}

#[test]
fn arbitrary_for_string() {
    assert_generates::<String>(["".into(), "a".into(), "aa".into(), "aaa".into()]);
}

#[test]
fn arbitrary_collection() {
    let x = [
        1, 2, 3, 4, 5, 6, 7, 8, 9, 1, 2, 3, 4, 5, 6, 7, 8, 9, 1, 2, 3, 4, 5, 6, 7, 8, 9, 8, 12,
    ];
    // &[u8] uses arbitrary_len -> arbitrary_byte_size (unchanged)
    assert_eq!(
        checked_arbitrary::<&[u8]>(&mut Unstructured::new(&x)).unwrap(),
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 1, 2, 3]
    );
    // Vec<u8>: fixed-size elements, byte_size=12, all bytes become elements
    assert_eq!(
        checked_arbitrary::<Vec<u8>>(&mut Unstructured::new(&x)).unwrap(),
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 1, 2, 3]
    );
    assert_eq!(
        &*checked_arbitrary::<Box<[u8]>>(&mut Unstructured::new(&x)).unwrap(),
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 1, 2, 3]
    );
    assert_eq!(
        &*checked_arbitrary::<Arc<[u8]>>(&mut Unstructured::new(&x)).unwrap(),
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 1, 2, 3]
    );
    assert_eq!(
        &*checked_arbitrary::<Rc<[u8]>>(&mut Unstructured::new(&x)).unwrap(),
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 1, 2, 3]
    );
    // Vec<u32>: fixed-size elements (4 bytes each), byte_size=12, 3 elements
    assert_eq!(
        checked_arbitrary::<Vec<u32>>(&mut Unstructured::new(&x)).unwrap(),
        &[0x04030201, 0x08070605, 0x03020109]
    );
    assert_eq!(
        checked_arbitrary::<String>(&mut Unstructured::new(&x)).unwrap(),
        "\x01\x02\x03\x04\x05\x06\x07\x08\x09\x01\x02\x03"
    );
}

#[test]
fn arbitrary_take_rest() {
    // Basic examples
    let x = [1, 2, 3, 4];
    assert_eq!(
        checked_arbitrary_take_rest::<&[u8]>(Unstructured::new(&x)).unwrap(),
        &[1, 2, 3, 4]
    );
    // Vec<u8> take_rest: fixed-size, all bytes become elements
    assert_eq!(
        checked_arbitrary_take_rest::<Vec<u8>>(Unstructured::new(&x)).unwrap(),
        &[1, 2, 3, 4]
    );
    assert_eq!(
        &*checked_arbitrary_take_rest::<Box<[u8]>>(Unstructured::new(&x)).unwrap(),
        &[1, 2, 3, 4]
    );
    assert_eq!(
        &*checked_arbitrary_take_rest::<Arc<[u8]>>(Unstructured::new(&x)).unwrap(),
        &[1, 2, 3, 4]
    );
    assert_eq!(
        &*checked_arbitrary_take_rest::<Rc<[u8]>>(Unstructured::new(&x)).unwrap(),
        &[1, 2, 3, 4]
    );
    // Vec<u32> take_rest: fixed-size (4 bytes each), one full element
    assert_eq!(
        checked_arbitrary_take_rest::<Vec<u32>>(Unstructured::new(&x)).unwrap(),
        &[0x04030201]
    );
    assert_eq!(
        checked_arbitrary_take_rest::<String>(Unstructured::new(&x)).unwrap(),
        "\x01\x02\x03\x04"
    );

    // Empty remainder
    assert_eq!(
        checked_arbitrary_take_rest::<&[u8]>(Unstructured::new(&[])).unwrap(),
        &[]
    );
    assert_eq!(
        checked_arbitrary_take_rest::<Vec<u8>>(Unstructured::new(&[])).unwrap(),
        &[]
    );

    // Cannot consume all but can consume part of the input
    assert_eq!(
        checked_arbitrary_take_rest::<String>(Unstructured::new(&[1, 0xFF, 2])).unwrap(),
        "\x01"
    );
}

#[test]
fn size_hint_for_tuples() {
    assert_eq!(
        (7, Some(7)),
        <(bool, u16, i32) as Arbitrary<'_>>::size_hint(0)
    );
    assert_eq!((1, None), <(u8, Vec<u8>) as Arbitrary>::size_hint(0));
}
