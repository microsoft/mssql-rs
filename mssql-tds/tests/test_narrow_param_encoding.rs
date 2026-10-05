// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Wire-byte tests for narrow (non-Unicode) character parameters.
//!
//! These exercise `TdsValueSerializer::serialize_string`'s `VARCHAR | CHAR |
//! TEXT` arm through the crate's public surface, asserting the bytes a
//! parameter actually reaches the wire as under a given collation.
//!
//! The arm used to resolve its code page from the collation's LCID alone,
//! ignoring both the UTF-8 flag and the SQL sort ID, so an inline parameter
//! disagreed with a streamed one and with fetched values under `_UTF8` and
//! CP437/CP850 collations (AB#48437).
//!
//! No server is required: the bytes come from the serializer itself via
//! `test_client_support::serialized_value_wire_bytes`. The in-crate unit tests
//! in `tds_value_serializer.rs` cover the same ground; these run the same
//! assertions from outside the crate, so a change that narrows the public
//! surface or reroutes serialization shows up here too.

use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::sql_string::{EncodingType, SqlString, encode_narrow};
use mssql_tds::test_client_support::{
    CAPTURE_PACKET_SIZE, TdsTypeContext, serialized_value_wire_bytes,
};
use mssql_tds::token::tokens::SqlCollation;

/// TDS type byte for VARCHAR (BIGVARCHAR).
const VARCHAR: u8 = 0xA7;
/// TDS type byte for CHAR (BIGCHAR) — fixed length, blank padded.
const CHAR: u8 = 0xAF;
/// TDS type byte for the legacy TEXT LOB.
const TEXT: u8 = 0x23;

/// A Windows collation: no UTF-8 flag, no sort ID, so the LCID decides.
/// LCID 0x0409 (US English) selects Windows-1252.
fn windows_1252() -> SqlCollation {
    SqlCollation {
        info: 0x0409,
        lcid_language_id: 0,
        col_flags: 0,
        sort_id: 0,
    }
}

/// A `_UTF8` collation over the same LCID, differing only in the `fUTF8` flag
/// (`col_flags & 0x40`).
fn utf8() -> SqlCollation {
    SqlCollation {
        info: 0x0409,
        lcid_language_id: 0,
        col_flags: 0x40,
        sort_id: 0,
    }
}

/// A SQL collation over the same LCID, differing only in the sort ID, so a byte
/// that differs from [`windows_1252`] is attributable to `sort_id` alone.
/// Sort ID 32 is CP437, 42 is CP850.
fn sort_id(sort_id: u8) -> SqlCollation {
    SqlCollation {
        info: 0x0409,
        lcid_language_id: 0,
        col_flags: 0,
        sort_id,
    }
}

/// Serializes `text` as a `varchar` parameter under `collation` and returns the
/// wire payload: a two-byte little-endian length prefix followed by the encoded
/// bytes.
fn varchar_payload(text: &str, collation: SqlCollation) -> Vec<u8> {
    payload(text, collation, VARCHAR, 8000, false, false)
}

/// Serializes `text` under an arbitrary narrow type/framing, so the same
/// collation resolution can be checked on every shape `serialize_string`'s
/// `VARCHAR | CHAR | TEXT` arm accepts.
fn payload(
    text: &str,
    collation: SqlCollation,
    tds_type: u8,
    max_size: usize,
    is_plp: bool,
    is_fixed_length: bool,
) -> Vec<u8> {
    let ctx = TdsTypeContext {
        tds_type,
        max_size,
        is_plp,
        is_fixed_length,
        precision: None,
        scale: None,
        collation: Some(collation),
        is_nullable: true,
    };
    // A UTF-8 source is what the ODBC layer hands down for a character
    // parameter. Note this reaches the serializer's UTF-8 passthrough under a
    // `_UTF8` collation, so for that collation the bytes are forwarded rather
    // than re-encoded; use [`utf16_payload`] to drive the resolver itself.
    let value = ColumnValues::String(SqlString::new(text.as_bytes().to_vec(), EncodingType::Utf8));
    serialized_value_wire_bytes(&value, &ctx).expect("serializes")
}

/// [`varchar_payload`] from a UTF-16LE source, so the value is decoded and
/// re-encoded through `encode_narrow_for_wire` rather than taking the UTF-8
/// passthrough.
///
/// Needed because that passthrough forwards an `EncodingType::Utf8` source
/// under a `_UTF8` collation without consulting `try_resolve_collation` at all.
/// A UTF-8-source assertion therefore cannot prove the resolver honours the
/// UTF-8 flag -- it would pass even if that branch were removed -- so the
/// collation-resolution claims are asserted from this side too.
fn utf16_payload(text: &str, collation: SqlCollation) -> Vec<u8> {
    let ctx = TdsTypeContext {
        tds_type: VARCHAR,
        max_size: 8000,
        is_plp: false,
        is_fixed_length: false,
        precision: None,
        scale: None,
        collation: Some(collation),
        is_nullable: true,
    };
    let utf16: Vec<u8> = text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let value = ColumnValues::String(SqlString::new(utf16, EncodingType::Utf16));
    serialized_value_wire_bytes(&value, &ctx).expect("serializes")
}

/// The UTF-8 flag wins over the LCID: U+00E9 is `C3 A9`, not the `E9` that
/// LCID 0x0409 alone would produce.
///
/// The length prefix is asserted with it - the encoded form is two bytes for
/// one character, so a prefix of 1 would frame the value short and desync the
/// parameter stream rather than merely mis-encoding it.
///
/// Asserted from both sources. The UTF-8 one takes the serializer's
/// passthrough, which forwards the bytes without consulting the resolver, so it
/// alone would pass even with the resolver's UTF-8 branch removed; the UTF-16
/// one is decoded and re-encoded, so it is what actually pins the branch.
#[test]
fn a_utf8_collation_encodes_utf8_rather_than_the_lcid_codepage() {
    assert_eq!(varchar_payload("\u{e9}", utf8()), b"\x02\x00\xc3\xa9");
    assert_eq!(
        utf16_payload("\u{e9}", utf8()),
        b"\x02\x00\xc3\xa9",
        "the resolver must honour the UTF-8 flag, not just the passthrough"
    );
}

/// A SQL sort ID wins over the LCID. CP437 and CP850 both put U+00E9 at `82`,
/// where Windows-1252 puts it at `E9`.
#[test]
fn a_sql_sort_id_selects_its_code_page_over_the_lcid() {
    for (id, what) in [(32u8, "CP437"), (42u8, "CP850")] {
        assert_eq!(
            varchar_payload("\u{e9}", sort_id(id)),
            b"\x01\x00\x82",
            "{what} (sort id {id})"
        );
    }
}

/// CP437 and CP850 are not interchangeable, so the sort ID has to select
/// between them rather than standing for "some OEM page".
///
/// Both put a character at `E0`, but a different one: U+03B1 GREEK SMALL ALPHA
/// in CP437, U+00D3 LATIN CAPITAL O WITH ACUTE in CP850. Each is unmappable in
/// the other page and becomes `?`, so swapping the two arms of the resolver
/// fails all four assertions.
#[test]
fn cp437_and_cp850_are_told_apart() {
    assert_eq!(
        varchar_payload("\u{3b1}", sort_id(32)),
        b"\x01\x00\xe0",
        "U+03B1 is CP437's E0"
    );
    assert_eq!(
        varchar_payload("\u{3b1}", sort_id(42)),
        b"\x01\x00?",
        "U+03B1 has no CP850 encoding"
    );
    assert_eq!(
        varchar_payload("\u{d3}", sort_id(42)),
        b"\x01\x00\xe0",
        "U+00D3 is CP850's E0"
    );
    assert_eq!(
        varchar_payload("\u{d3}", sort_id(32)),
        b"\x01\x00?",
        "U+00D3 has no CP437 encoding"
    );
}

/// The control: an ordinary Windows collation still resolves through the LCID
/// exactly as before, so the fix adds branches ahead of that path rather than
/// replacing it.
#[test]
fn a_windows_collation_still_encodes_through_the_lcid_codepage() {
    assert_eq!(varchar_payload("\u{e9}", windows_1252()), b"\x01\x00\xe9");
}

/// Buffered and streamed writes must agree. `encode_narrow` is what the
/// data-at-execution path streams through; the inline arm now resolves the same
/// way, so one value under one collation reaches the wire as the same bytes
/// whichever route it took. Before the fix the two disagreed under exactly the
/// collations above.
#[test]
fn the_inline_arm_agrees_with_the_streamed_encoder() {
    let probes = ["\u{e9}", "\u{3b1}", "\u{d3}", "Caf\u{e9} \u{65e5}", "plain"];
    let collations = [
        ("utf8", utf8()),
        ("cp437", sort_id(32)),
        ("cp850", sort_id(42)),
        ("windows-1252", windows_1252()),
    ];
    for (what, collation) in collations {
        for probe in probes {
            let inline = varchar_payload(probe, collation);
            let streamed = encode_narrow(probe, collation).bytes;
            // The inline payload carries a two-byte length prefix; the streamed
            // one is bare bytes. Compare the bodies, and check the prefix counts
            // them.
            let prefix = u16::from_le_bytes([inline[0], inline[1]]) as usize;
            assert_eq!(
                &inline[2..],
                streamed.as_slice(),
                "{what}: inline and streamed bytes differ for {probe:?}"
            );
            assert_eq!(
                prefix,
                streamed.len(),
                "{what}: length prefix does not count the encoded bytes for {probe:?}"
            );
        }
    }
}

/// A multibyte UTF-8 result must be framed by byte count, not character count.
/// U+65E5 is three bytes under a UTF-8 collation and unmappable in every
/// single-byte page, so this also shows the UTF-8 arm avoids the substitution
/// the Windows-1252 control makes.
///
/// The UTF-16 source is what reaches the resolver; see
/// [`a_utf8_collation_encodes_utf8_rather_than_the_lcid_codepage`].
#[test]
fn a_utf8_collation_frames_multibyte_output_by_byte_count() {
    assert_eq!(varchar_payload("\u{65e5}", utf8()), b"\x03\x00\xe6\x97\xa5");
    assert_eq!(
        utf16_payload("\u{65e5}", utf8()),
        b"\x03\x00\xe6\x97\xa5",
        "the resolver must frame the re-encoded form by byte count"
    );
    assert_eq!(varchar_payload("\u{65e5}", windows_1252()), b"\x01\x00?");
}

/// ASCII is identical in all four code pages, so it must round-trip unchanged
/// whichever collation is in force. Guards against a resolver change that
/// accidentally routes plain text through a transform.
#[test]
fn ascii_is_unchanged_under_every_collation() {
    for (what, collation) in [
        ("utf8", utf8()),
        ("cp437", sort_id(32)),
        ("cp850", sort_id(42)),
        ("windows-1252", windows_1252()),
    ] {
        assert_eq!(varchar_payload("abc", collation), b"\x03\x00abc", "{what}");
    }
}

/// The resolver is shared by every narrow shape, not just the bounded
/// `varchar(n)` the tests above use. `CHAR` pads to its declared length and
/// `TEXT` carries a LOB header, so each frames the *same* encoded bytes
/// differently - the encoding must not vary with the framing.
///
/// U+00E9 is `82` under CP437/CP850 and `E9` under Windows-1252, so a form that
/// regressed to the LCID-only resolver shows up as a wrong byte inside its own
/// framing rather than as a length or header difference.
#[test]
fn every_narrow_form_resolves_the_same_collation() {
    // CHAR(4): fixed length, so the encoded byte is followed by blank padding
    // to the declared width and there is no length prefix.
    assert_eq!(
        payload("\u{e9}", sort_id(32), CHAR, 4, false, true),
        b"\x82\x20\x20\x20",
        "char(4) under CP437"
    );
    assert_eq!(
        payload("\u{e9}", windows_1252(), CHAR, 4, false, true),
        b"\xe9\x20\x20\x20",
        "char(4) under Windows-1252"
    );

    // TEXT: 0x10, a 16-byte textptr and an 8-byte timestamp (all 0xFF), then a
    // four-byte little-endian length and the data.
    let mut expected_text = vec![0x10u8];
    expected_text.extend(std::iter::repeat_n(0xFFu8, 24));
    expected_text.extend([0x01, 0x00, 0x00, 0x00, 0x82]);
    assert_eq!(
        payload("\u{e9}", sort_id(32), TEXT, 8000, false, false),
        expected_text,
        "text under CP437"
    );

    // varchar(max): PLP framing - an unknown-length header, one chunk, then the
    // terminator. The bytes inside the chunk are the same as the bounded form's.
    let mut expected_plp = vec![0xFEu8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
    expected_plp.extend([0x01, 0x00, 0x00, 0x00, 0x82]);
    expected_plp.extend([0x00, 0x00, 0x00, 0x00]);
    assert_eq!(
        payload("\u{e9}", sort_id(32), VARCHAR, 0, true, false),
        expected_plp,
        "varchar(max) under CP437"
    );
}

/// Every narrow form must agree with the streamed encoder too, not just the
/// bounded `varchar(n)` checked above. Compares the encoded body each form
/// carries - stripped of its own framing - against `encode_narrow`.
#[test]
fn every_narrow_form_agrees_with_the_streamed_encoder() {
    for (what, collation) in [
        ("utf8", utf8()),
        ("cp437", sort_id(32)),
        ("cp850", sort_id(42)),
        ("windows-1252", windows_1252()),
    ] {
        for probe in ["\u{e9}", "\u{3b1}", "\u{d3}", "abc"] {
            let streamed = encode_narrow(probe, collation).bytes;

            // varchar(n): 2-byte length prefix, then the body.
            let bounded = payload(probe, collation, VARCHAR, 8000, false, false);
            assert_eq!(
                &bounded[2..],
                streamed.as_slice(),
                "{what} varchar {probe:?}"
            );

            // text: 25-byte header, 4-byte length, then the body.
            let text = payload(probe, collation, TEXT, 8000, false, false);
            assert_eq!(&text[29..], streamed.as_slice(), "{what} text {probe:?}");

            // varchar(max): 8-byte PLP header, 4-byte chunk length, body, then a
            // 4-byte terminator.
            let plp = payload(probe, collation, VARCHAR, 0, true, false);
            assert_eq!(
                &plp[12..plp.len() - 4],
                streamed.as_slice(),
                "{what} varchar(max) {probe:?}"
            );
        }
    }
}

/// The `sql_variant` narrow path resolves its own bytes through `encode_narrow`
/// directly (`resolve_narrow_wire_bytes`), so it was already correct and the
/// resolver change must not have rerouted it. Pinned because the two paths now
/// share a helper and could be merged by mistake.
///
/// The discriminating probe is the *unmapped* collation. Since AB#48437 the two
/// helpers resolve identically for every collation they both map, so a mapped
/// probe cannot tell them apart — the only observable difference left is the
/// fallback: `encode_narrow` defaults to Windows-1252, where U+0080 is
/// unmappable and substitutes to `3F`, while the serializer arm's Latin-1
/// mapping passes it through as `80`. Verified by mutation: rewriting
/// `resolve_narrow_wire_bytes` to call `encode_narrow_for_wire` leaves the
/// CP437 assertion below green and fails only the unmapped one.
#[test]
fn the_sql_variant_narrow_path_still_encodes_through_the_shared_encoder() {
    const SQL_VARIANT: u8 = 0x62;

    // A collation naming no encoding this crate maps, so the two helpers'
    // fallbacks diverge and the byte identifies which one ran.
    let unmapped = SqlCollation {
        info: 0x000F_FFFF,
        lcid_language_id: 0,
        col_flags: 0,
        sort_id: 0,
    };
    let bytes = payload("\u{80}", unmapped, SQL_VARIANT, 8009, false, false);
    assert_eq!(
        bytes.last().copied(),
        Some(0x3F),
        "sql_variant must keep encode_narrow's Windows-1252 fallback, not the \
         serializer arm's Latin-1 one: {bytes:02X?}"
    );

    // The sort ID is still honoured on a mapped collation. Not discriminating
    // between the two helpers, but a real check of the variant path's resolver.
    let bytes = payload("\u{e9}", sort_id(32), SQL_VARIANT, 8009, false, false);
    assert_eq!(
        bytes.last().copied(),
        Some(0x82),
        "sql_variant resolved a different code page than CP437: {bytes:02X?}"
    );
    assert_eq!(
        bytes.last().copied(),
        encode_narrow("\u{e9}", sort_id(32)).bytes.last().copied(),
        "sql_variant and the streamed encoder disagree"
    );
}

/// A collation naming no encoding this crate maps keeps the serializer's own
/// Latin-1 fallback rather than inheriting `encode_narrow`'s Windows-1252 one.
/// Deliberately not unified: that is a behaviour change on a path the resolver
/// fix does not otherwise touch.
///
/// U+0080 separates the two - it is its own byte under the Latin-1 mapping and
/// unmappable in Windows-1252, whose `80` is the Euro sign.
#[test]
fn an_unmapped_collation_keeps_the_latin1_fallback() {
    let unmapped = SqlCollation {
        info: 0x000F_FFFF,
        lcid_language_id: 0,
        col_flags: 0,
        sort_id: 0,
    };
    assert_eq!(
        varchar_payload("\u{80}", unmapped),
        b"\x01\x00\x80",
        "Latin-1 passes U+0080 through"
    );
    // Windows-1252 would have substituted instead, which is what makes the
    // assertion above about the fallback rather than about U+0080.
    assert_eq!(encode_narrow("\u{80}", unmapped).bytes, b"?");
}

/// A value larger than one TDS packet must come back whole.
///
/// `PacketWriter` flushes each full packet through `NetworkWriter::send` and
/// then reuses its buffer for the remainder, so a helper that read only that
/// buffer would return the final fragment and silently pass. The value is sized
/// against [`CAPTURE_PACKET_SIZE`] rather than a literal, and the assertion
/// below enforces that, so raising the helper's packet size fails loudly here
/// instead of quietly reducing this to a single-packet test.
///
/// Asserts against `encode_narrow` rather than a literal: the point is that
/// nothing is dropped or duplicated at a boundary, not what any single byte is.
#[test]
fn a_value_spanning_multiple_packets_is_returned_whole() {
    // Non-ASCII so every byte also exercises the resolver, and a repeating unit
    // that is one byte under CP437 so the expected length is predictable.
    let text = "\u{e9}".repeat(12_000);
    let streamed = encode_narrow(&text, sort_id(32));
    assert_eq!(
        streamed.bytes.len(),
        12_000,
        "CP437 encodes U+00E9 to one byte"
    );
    assert!(
        streamed.bytes.len() > CAPTURE_PACKET_SIZE as usize,
        "probe no longer spans a packet boundary"
    );

    // varchar(max): PLP framing - 8-byte header, 4-byte chunk length, body,
    // 4-byte terminator.
    let plp = payload(&text, sort_id(32), VARCHAR, 0, true, false);
    let body = &plp[12..plp.len() - 4];
    assert_eq!(
        body.len(),
        streamed.bytes.len(),
        "multi-packet value was truncated or duplicated"
    );
    assert_eq!(body, streamed.bytes.as_slice());
}

/// The same check for the bounded form, which frames with a two-byte prefix
/// rather than PLP. The prefix must still count the whole value.
#[test]
fn a_bounded_value_spanning_multiple_packets_keeps_its_length_prefix() {
    let text = "\u{e9}".repeat(6_000);
    let encoded = encode_narrow(&text, sort_id(32));
    assert!(
        encoded.bytes.len() > CAPTURE_PACKET_SIZE as usize,
        "probe no longer spans a packet boundary"
    );
    let bounded = payload(&text, sort_id(32), VARCHAR, 8000, false, false);
    let prefix = u16::from_le_bytes([bounded[0], bounded[1]]) as usize;
    assert_eq!(prefix, 6_000);
    assert_eq!(&bounded[2..], encoded.bytes.as_slice());
}

/// A lone UTF-16 surrogate reaching a *single-byte* narrow target is
/// substituted.
///
/// Scoped to Windows-1252 deliberately. A `_UTF8` collation is also a narrow
/// target, but U+FFFD is representable there, so it is *not* substituted —
/// that is the next test, and stating the contract in general terms here would
/// contradict it.
///
/// Driven through `serialize_value` with a genuinely malformed UTF-16 source,
/// so the decode is the one the serializer performs rather than a repair this
/// test performed for it: `to_utf8_string` resolves `EncodingType::Utf16`
/// through `encoding_rs`, which yields U+FFFD, and a single-byte collation
/// then has no byte for that.
///
/// Measured against retail msodbcsql18: binding `a<D800>b` to a varchar stored
/// `61 3F 62` with `SQL_SUCCESS` and no diagnostic, and the engine's own
/// `CAST(NCHAR(97)+NCHAR(55296)+NCHAR(98) AS VARCHAR)` produced the identical
/// bytes.
///
/// Asserts the payload only; the loss flag is pinned by
/// `a_lone_surrogate_substitutes_and_marks_the_message` in
/// `tds_value_serializer.rs`, which can read it off the `PacketWriter`.
#[test]
fn a_lone_surrogate_reaching_a_narrow_target_is_substituted() {
    assert_eq!(
        lone_surrogate_payload(windows_1252()),
        b"\x03\x00a?b",
        "matches msodbcsql and the engine: one '?' per unpaired surrogate"
    );
}

/// The same lone surrogate under a `_UTF8` collation, where it is *not*
/// substituted: U+FFFD is representable in UTF-8, so the repaired character
/// survives as its own three bytes and nothing is lost.
///
/// This changed with AB#48437. Before it, the inline arm resolved the LCID
/// alone and sent `61 3F 62` with the loss flagged; now it agrees with the
/// streamed `encode_narrow` path, which has behaved this way all along.
/// `WideCharToMultiByte(CP_UTF8, ...)` without `WC_ERR_INVALID_CHARS` also
/// yields `EF BF BD`, so msodbcsql is expected to agree — that leg is inferred
/// from the API contract rather than measured against a `_UTF8` database.
#[test]
fn a_lone_surrogate_under_a_utf8_collation_is_not_substituted() {
    assert_eq!(
        lone_surrogate_payload(utf8()),
        b"\x05\x00a\xef\xbf\xbdb",
        "U+FFFD is representable in UTF-8, so it is not substituted"
    );
}

/// Serializes `'a'`, an unpaired high surrogate, `'b'` as raw UTF-16LE under
/// `collation`. The source is deliberately not decodable to a scalar, so the
/// repair to U+FFFD is the serializer's own.
fn lone_surrogate_payload(collation: SqlCollation) -> Vec<u8> {
    let utf16: Vec<u8> = [0x0061u16, 0xD800, 0x0062]
        .iter()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    let ctx = TdsTypeContext {
        tds_type: VARCHAR,
        max_size: 8000,
        is_plp: false,
        is_fixed_length: false,
        precision: None,
        scale: None,
        collation: Some(collation),
        is_nullable: true,
    };
    let value = ColumnValues::String(SqlString::new(utf16, EncodingType::Utf16));
    serialized_value_wire_bytes(&value, &ctx).expect("serializes")
}

/// The resolver's `encoding_rs` arm carries 13 code pages besides the two OEM
/// special cases, and this change routes inline parameters through it for the
/// first time. Sort ID 105 is CP1251, where U+0410 is `C0` and is unmappable in
/// the LCID's Windows-1252 — so a regression to LCID-only resolution shows up
/// as a substitution rather than a wrong-but-plausible byte.
#[test]
fn a_sort_id_resolving_through_encoding_rs_is_honored() {
    assert_eq!(
        varchar_payload("\u{410}", sort_id(105)),
        b"\x01\x00\xc0",
        "CP1251 (sort id 105)"
    );
    assert_eq!(
        varchar_payload("\u{410}", windows_1252()),
        b"\x01\x00?",
        "the same LCID without the sort ID cannot represent it"
    );
}

/// A DBCS sort ID through the same arm, where one character is two bytes: the
/// length prefix has to count encoded bytes, not characters. Sort ID 192 is
/// CP932, where U+65E5 is `93 FA`.
#[test]
fn a_dbcs_sort_id_frames_by_encoded_byte_count() {
    assert_eq!(
        varchar_payload("\u{65e5}", sort_id(192)),
        b"\x02\x00\x93\xfa",
        "CP932 (sort id 192)"
    );
}

/// Honoring the collation makes a value grow, and `serialize_char_varchar_direct`
/// measures the *encoded* bytes against the declared length — so a value that
/// fit under the LCID's single-byte page can now overflow it.
///
/// Under a `_UTF8` collation U+00E9 is two bytes, so `varchar(1)` is rejected
/// where before this change it encoded to one CP1252 byte and succeeded. The
/// rejection is correct in that the value genuinely does not fit, but it
/// surfaces as an opaque `UsageError` (`HY000` at the ODBC layer) rather than
/// the `22001` msodbcsql reports, because the ODBC layer measures the parameter
/// in UTF-16 units before the collation is known (`param_convert.rs`,
/// AB#47584).
///
/// Pinned so the regression direction stays visible: when AB#47584 gives the
/// ODBC layer the collation, this assertion is what should change, and it
/// should change to `22001` rather than to silent acceptance.
#[test]
fn a_utf8_collation_can_push_a_value_past_its_declared_length() {
    let err = try_varchar_payload("\u{e9}", utf8(), 1)
        .expect_err("two encoded bytes do not fit varchar(1)");
    assert!(
        err.to_string().contains("exceeds schema size"),
        "expected the length guard, got: {err}"
    );

    // The same binding under the LCID's single-byte page still fits, which is
    // what makes this about the collation rather than about the value.
    assert_eq!(
        varchar_payload_sized("\u{e9}", windows_1252(), 1),
        b"\x01\x00\xe9"
    );
}

/// [`varchar_payload`] against a declared length, returning the serializer's
/// error instead of panicking on it.
fn try_varchar_payload(
    text: &str,
    collation: SqlCollation,
    max_size: usize,
) -> Result<Vec<u8>, mssql_tds::error::Error> {
    let ctx = TdsTypeContext {
        tds_type: VARCHAR,
        max_size,
        is_plp: false,
        is_fixed_length: false,
        precision: None,
        scale: None,
        collation: Some(collation),
        is_nullable: true,
    };
    let value = ColumnValues::String(SqlString::new(text.as_bytes().to_vec(), EncodingType::Utf8));
    serialized_value_wire_bytes(&value, &ctx)
}

/// [`try_varchar_payload`] for a case expected to serialize.
fn varchar_payload_sized(text: &str, collation: SqlCollation, max_size: usize) -> Vec<u8> {
    try_varchar_payload(text, collation, max_size).expect("serializes")
}
