// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The subset of ASN.1 BER (X.690) that a Kerberos ticket and LDAP's
//! messages (RFC 4511 section 5.1) use: definite lengths only, read without
//! trusting a length past the input, and written in their shortest form.

/// The content of the element at the start of `input` when its tag is
/// `tag`, and what follows the element.
pub(super) fn element(input: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    match next(input)? {
        (found, content, rest) if found == tag => Some((content, rest)),
        _ => None,
    }
}

/// The element at the start of `input`: its tag, its content, and what
/// follows it.
pub(super) fn next(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (tag, header, length) = header(input)?;
    let rest = &input[header..];
    (rest.len() >= length).then(|| {
        let (content, rest) = rest.split_at(length);
        (tag, content, rest)
    })
}

/// The length of the whole element `input` starts with, once its header is
/// complete: `Ok(None)` while more input is needed, `Err(())` for a header
/// no encoding here uses (a length of more than four bytes, or indefinite).
pub(super) fn framed_length(input: &[u8]) -> Result<Option<usize>, ()> {
    match input {
        [] | [_] => Ok(None),
        [_, first, rest @ ..] => {
            let count = usize::from(first & 0x7F);
            if *first >= 0x80 && (count == 0 || count > 4) {
                return Err(());
            }
            if *first >= 0x80 && rest.len() < count {
                return Ok(None);
            }
            header(input)
                .map(|(_, header, length)| Some(header + length))
                .ok_or(())
        }
    }
}

/// The tag, the header's length and the content's length of the element at
/// the start of `input`.
fn header(input: &[u8]) -> Option<(u8, usize, usize)> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    if first < 0x80 {
        return Some((tag, 2, usize::from(first)));
    }
    let count = usize::from(first & 0x7F);
    if count == 0 || count > 4 || rest.len() < count {
        return None;
    }
    let length = rest[..count]
        .iter()
        .fold(0usize, |length, byte| (length << 8) | usize::from(*byte));
    Some((tag, 2 + count, length))
}

/// The content of the field `tag` among a SEQUENCE's.
pub(super) fn field(mut fields: &[u8], tag: u8) -> Option<&[u8]> {
    while !fields.is_empty() {
        let (found, content, rest) = next(fields)?;
        if found == tag {
            return Some(content);
        }
        fields = rest;
    }
    None
}

/// The INTEGER at the start of `input`, when it fits 64 bits.
pub(super) fn integer(input: &[u8]) -> Option<i64> {
    tagged_integer(input, 0x02)
}

/// The two's-complement number with `tag` (an INTEGER, or an ENUMERATED's
/// 0x0A) at the start of `input`, when it fits 64 bits.
pub(super) fn tagged_integer(input: &[u8], tag: u8) -> Option<i64> {
    let (bytes, _) = element(input, tag)?;
    if bytes.is_empty() || bytes.len() > 8 {
        return None;
    }
    let sign = if bytes[0] & 0x80 == 0 { 0 } else { -1 };
    Some(
        bytes
            .iter()
            .fold(sign, |value: i64, byte| (value << 8) | i64::from(*byte)),
    )
}

/// An element with `tag` around `content`.
pub(super) fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 6);
    out.push(tag);
    match u8::try_from(content.len()) {
        Ok(length) if length < 0x80 => out.push(length),
        _ => {
            let length = (content.len() as u32).to_be_bytes();
            let skip = length.iter().take_while(|byte| **byte == 0).count();
            out.push(0x80 | (4 - skip) as u8);
            out.extend_from_slice(&length[skip..]);
        }
    }
    out.extend_from_slice(content);
    out
}

/// A two's-complement number with `tag` (0x02 INTEGER, 0x0A ENUMERATED) in
/// as few bytes as hold it.
pub(super) fn number(tag: u8, value: i64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let mut start = 0;
    while start < 7
        && ((bytes[start] == 0 && bytes[start + 1] & 0x80 == 0)
            || (bytes[start] == 0xFF && bytes[start + 1] & 0x80 != 0))
    {
        start += 1;
    }
    tlv(tag, &bytes[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_take_as_few_bytes_as_hold_them_and_read_back() {
        for (value, encoded) in [
            (0, vec![0x02, 0x01, 0x00]),
            (127, vec![0x02, 0x01, 0x7F]),
            (128, vec![0x02, 0x02, 0x00, 0x80]),
            (-1, vec![0x02, 0x01, 0xFF]),
            (-129, vec![0x02, 0x02, 0xFF, 0x7F]),
            (
                i64::from(u32::MAX),
                vec![0x02, 0x05, 0x00, 0xFF, 0xFF, 0xFF, 0xFF],
            ),
        ] {
            assert_eq!(number(0x02, value), encoded, "{value}");
            assert_eq!(integer(&encoded), Some(value));
        }
        assert_eq!(tagged_integer(&number(0x0A, 14), 0x0A), Some(14));
        assert_eq!(integer(&number(0x0A, 14)), None, "another tag");
    }

    #[test]
    fn long_contents_get_a_long_form_length() {
        let content = vec![7; 300];
        let encoded = tlv(0x04, &content);
        assert_eq!(&encoded[..4], [0x04, 0x82, 0x01, 0x2C]);
        assert_eq!(element(&encoded, 0x04), Some((&content[..], &[][..])));
        assert_eq!(framed_length(&encoded), Ok(Some(304)));
    }

    #[test]
    fn a_frame_is_known_once_its_header_is() {
        let encoded = tlv(0x30, &[1; 200]);
        assert_eq!(&encoded[..3], [0x30, 0x81, 0xC8]);
        assert_eq!(framed_length(&encoded[..1]), Ok(None));
        assert_eq!(
            framed_length(&encoded[..2]),
            Ok(None),
            "a length byte follows"
        );
        assert_eq!(framed_length(&encoded[..3]), Ok(Some(203)));
        assert_eq!(framed_length(&encoded[..4]), Ok(Some(203)));
        // Indefinite, or longer than four bytes: not an encoding read here.
        assert_eq!(framed_length(&[0x30, 0x80]), Err(()));
        assert_eq!(framed_length(&[0x30, 0x85, 1, 1, 1, 1, 1]), Err(()));
    }

    #[test]
    fn nothing_is_read_past_the_input() {
        let encoded = tlv(0x30, &tlv(0x04, b"value"));
        for end in 0..encoded.len() {
            assert_eq!(next(&encoded[..end]), None, "cut at {end}");
        }
        assert_eq!(element(&[0x04, 0x84, 0xFF, 0xFF, 0xFF, 0xFF], 0x04), None);
        let (content, rest) = element(&encoded, 0x30).unwrap();
        assert!(rest.is_empty());
        assert_eq!(field(content, 0x04), Some(&b"value"[..]));
        assert_eq!(field(content, 0x05), None);
    }
}
