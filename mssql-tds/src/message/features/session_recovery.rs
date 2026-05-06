// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use async_trait::async_trait;

use crate::connection::session_recovery::SessionRecoveryData;
use crate::core::TdsResult;
use crate::io::packet_writer::{PacketWriter, TdsPacketWriter};
use crate::message::login::{Feature, FeatureExtension};

/// Session Recovery (Idle Connection Resiliency) feature extension (Feature ID 0x01).
///
/// On initial login (`recovery_data == None`): serializes as feature ID byte + data_length 0.
/// On reconnection (`recovery_data == Some`): serializes recovery state per the TDS wire format.
#[derive(Debug, Clone)]
pub(crate) struct SessionRecoveryFeature {
    acknowledged: bool,
    connect_retry_count: u32,
    /// None for initial login, Some for reconnection.
    /// Boxed to avoid ~16KB on the stack (two 256-element arrays).
    recovery_data: Option<Box<SessionRecoveryData>>,
    /// Initial session state data received from FEATUREEXTACK, stored for
    /// later use by RecoveryContext.
    pub(crate) initial_state_data: Option<Vec<u8>>,
}

impl SessionRecoveryFeature {
    /// Create a new feature for initial login negotiation.
    pub fn new(connect_retry_count: u32) -> Self {
        Self {
            acknowledged: false,
            connect_retry_count,
            recovery_data: None,
            initial_state_data: None,
        }
    }

    /// Create a feature pre-populated with recovery data for reconnection.
    pub fn new_for_reconnection(recovery_data: Box<SessionRecoveryData>) -> Self {
        Self {
            acknowledged: false,
            connect_retry_count: 1, // must be > 0 for is_requested()
            recovery_data: Some(recovery_data),
            initial_state_data: None,
        }
    }
}

impl From<Box<SessionRecoveryData>> for SessionRecoveryFeature {
    fn from(recovery_data: Box<SessionRecoveryData>) -> Self {
        Self::new_for_reconnection(recovery_data)
    }
}

use crate::connection::session_recovery::SessionStateRecord;
use crate::token::tokens::SqlCollation;

/// Write a B_VARCHAR: 1-byte character count + UTF-16LE encoded string.
async fn write_b_varchar(packet_writer: &mut PacketWriter<'_>, value: &str) -> TdsResult<()> {
    let char_count = value.encode_utf16().count();
    packet_writer.write_byte_async(char_count as u8).await?;
    if char_count > 0 {
        packet_writer.write_string_unicode_async(value).await?;
    }
    Ok(())
}

/// Write a collation value: 1-byte length + optional 5 raw bytes (4 info + 1 sort_id).
async fn write_collation(
    packet_writer: &mut PacketWriter<'_>,
    collation: &SqlCollation,
) -> TdsResult<()> {
    if *collation == SqlCollation::default() {
        packet_writer.write_byte_async(0).await?;
    } else {
        packet_writer.write_byte_async(5).await?;
        packet_writer.write_u32_async(collation.info).await?;
        packet_writer.write_byte_async(collation.sort_id).await?;
    }
    Ok(())
}

/// Write all non-null state entries from a 256-element array.
///
/// Each entry: state_id (1 byte) + length encoding + data bytes.
/// Length encoding: if data.len() < 0xFF → 1 byte; else → 0xFF marker + 4-byte DWORD.
async fn write_state_entries(
    packet_writer: &mut PacketWriter<'_>,
    states: &[Option<SessionStateRecord>; 256],
) -> TdsResult<()> {
    for (i, entry) in states.iter().enumerate() {
        if let Some(record) = entry.as_ref() {
            packet_writer.write_byte_async(i as u8).await?;
            let data_len = record.data.len();
            if data_len < 0xFF {
                packet_writer.write_byte_async(data_len as u8).await?;
            } else {
                packet_writer.write_byte_async(0xFF).await?;
                packet_writer.write_u32_async(data_len as u32).await?;
            }
            packet_writer.write_async(&record.data).await?;
        }
    }
    Ok(())
}

#[async_trait]
impl Feature for SessionRecoveryFeature {
    fn feature_identifier(&self) -> FeatureExtension {
        FeatureExtension::SRecovery
    }

    fn is_requested(&self) -> bool {
        self.connect_retry_count > 0
    }

    fn data_length(&self) -> i32 {
        if let Some(ref data) = self.recovery_data {
            // Reconnection: feature_id(1) + total_data_length(4) + initial_length(4)
            // + initial_block + current_length(4) + delta_block
            (1 + 4 + 4 + data.initial_block_length() + 4 + data.delta_block_length()) as i32
        } else {
            // Initial login: feature_id byte + 0-length i32
            (size_of::<u8>() + size_of::<i32>()) as i32
        }
    }

    async fn serialize(&self, packet_writer: &mut PacketWriter) -> TdsResult<()> {
        packet_writer
            .write_byte_async(self.feature_identifier().as_u8())
            .await?;

        if let Some(ref data) = self.recovery_data {
            // Total data length = 8 + initialLen + deltaLen
            packet_writer
                .write_u32_async(data.total_data_length())
                .await?;

            // ── Initial block ──
            packet_writer
                .write_u32_async(data.initial_block_length())
                .await?;
            // B_VARCHAR: initial_database
            write_b_varchar(packet_writer, &data.initial_database).await?;
            // Collation
            write_collation(packet_writer, &data.initial_collation).await?;
            // B_VARCHAR: initial_language
            write_b_varchar(packet_writer, &data.initial_language).await?;
            // State entries
            write_state_entries(packet_writer, &data.initial_state).await?;

            // ── Delta block ──
            packet_writer
                .write_u32_async(data.delta_block_length())
                .await?;
            // Database: if same as initial → 0, else B_VARCHAR
            if data.database == data.initial_database {
                packet_writer.write_byte_async(0).await?;
            } else {
                write_b_varchar(packet_writer, &data.database).await?;
            }
            // Collation: if same → 0, else full
            if data.collation == data.initial_collation {
                packet_writer.write_byte_async(0).await?;
            } else {
                write_collation(packet_writer, &data.collation).await?;
            }
            // Language: if same → 0, else B_VARCHAR
            if data.language == data.initial_language {
                packet_writer.write_byte_async(0).await?;
            } else {
                write_b_varchar(packet_writer, &data.language).await?;
            }
            // Delta state entries
            write_state_entries(packet_writer, &data.delta).await?;
        } else {
            // Initial login: data_length 0
            packet_writer.write_i32_async(0).await?;
        }
        Ok(())
    }

    fn deserialize(&mut self, data: &[u8]) -> TdsResult<()> {
        // Store the raw FEATUREEXTACK data for later processing by RecoveryContext.
        // The server sends initial session state in this payload.
        if !data.is_empty() {
            self.initial_state_data = Some(data.to_vec());
        }
        Ok(())
    }

    fn is_acknowledged(&self) -> bool {
        self.acknowledged
    }

    fn set_acknowledged(&mut self, acknowledged: bool) {
        self.acknowledged = acknowledged;
    }

    fn clone_box(&self) -> Box<dyn Feature> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_identifier_is_srecovery() {
        let feature = SessionRecoveryFeature::new(1);
        assert_eq!(feature.feature_identifier(), FeatureExtension::SRecovery);
        assert_eq!(feature.feature_identifier().as_u8(), 0x01);
    }

    #[test]
    fn is_requested_when_retry_count_positive() {
        assert!(SessionRecoveryFeature::new(1).is_requested());
        assert!(SessionRecoveryFeature::new(255).is_requested());
    }

    #[test]
    fn not_requested_when_retry_count_zero() {
        assert!(!SessionRecoveryFeature::new(0).is_requested());
    }

    #[test]
    fn data_length_initial_login() {
        let feature = SessionRecoveryFeature::new(1);
        // feature_id (1 byte) + data_length i32 (4 bytes) = 5
        assert_eq!(feature.data_length(), 5);
    }

    #[test]
    fn deserialize_stores_initial_state_data() {
        let mut feature = SessionRecoveryFeature::new(1);
        assert!(feature.initial_state_data.is_none());

        let data = vec![0x01, 0x02, 0x03];
        feature.deserialize(&data).unwrap();
        assert_eq!(feature.initial_state_data.as_ref().unwrap(), &data);
    }

    #[test]
    fn deserialize_empty_data_leaves_none() {
        let mut feature = SessionRecoveryFeature::new(1);
        feature.deserialize(&[]).unwrap();
        assert!(feature.initial_state_data.is_none());
    }

    #[test]
    fn acknowledged_defaults_to_false() {
        let feature = SessionRecoveryFeature::new(1);
        assert!(!feature.is_acknowledged());
    }

    #[test]
    fn set_acknowledged_works() {
        let mut feature = SessionRecoveryFeature::new(1);
        feature.set_acknowledged(true);
        assert!(feature.is_acknowledged());
    }

    #[test]
    fn clone_box_produces_independent_copy() {
        let mut feature = SessionRecoveryFeature::new(1);
        feature.set_acknowledged(true);
        let cloned = feature.clone_box();
        assert!(cloned.is_acknowledged());
        assert_eq!(cloned.feature_identifier(), FeatureExtension::SRecovery);
    }

    #[test]
    fn new_for_reconnection_is_requested() {
        use crate::connection::session_recovery::SessionRecoveryData;
        use crate::token::tokens::SqlCollation;

        let data = SessionRecoveryData {
            initial_state: std::array::from_fn(|_| None),
            delta: std::array::from_fn(|_| None),
            initial_database: "testdb".to_string(),
            initial_language: "us_english".to_string(),
            initial_collation: SqlCollation::default(),
            database: "testdb".to_string(),
            language: "us_english".to_string(),
            collation: SqlCollation::default(),
        };
        let feature = SessionRecoveryFeature::new_for_reconnection(Box::new(data));
        assert!(feature.is_requested());
        assert!(feature.recovery_data.is_some());
    }

    // ── Reconnection serialization tests ──

    use crate::connection::session_recovery::{SessionRecoveryData, SessionStateRecord};

    fn make_recovery_data_empty() -> SessionRecoveryData {
        SessionRecoveryData {
            initial_state: std::array::from_fn(|_| None),
            delta: std::array::from_fn(|_| None),
            initial_database: String::new(),
            initial_language: String::new(),
            initial_collation: SqlCollation::default(),
            database: String::new(),
            language: String::new(),
            collation: SqlCollation::default(),
        }
    }

    fn make_collation(info: u32, sort_id: u8) -> SqlCollation {
        SqlCollation {
            info,
            lcid_language_id: (info & 0x000FFFFF) as i32,
            col_flags: ((info >> 20) & 0xFF) as u8,
            sort_id,
        }
    }

    #[test]
    fn initial_block_length_empty_strings_default_collation() {
        let data = make_recovery_data_empty();
        // B_VARCHAR("") = 1 byte (char_count 0)
        // Collation(default) = 1 byte (length 0)
        // B_VARCHAR("") = 1 byte (char_count 0)
        // No state entries
        assert_eq!(data.initial_block_length(), 3);
    }

    #[test]
    fn initial_block_length_with_strings() {
        let mut data = make_recovery_data_empty();
        data.initial_database = "master".to_string(); // 6 chars → 1 + 12 = 13
        data.initial_language = "us_english".to_string(); // 10 chars → 1 + 20 = 21
        // default collation → 1 byte
        assert_eq!(data.initial_block_length(), 13 + 1 + 21);
    }

    #[test]
    fn initial_block_length_with_collation() {
        let mut data = make_recovery_data_empty();
        data.initial_collation = make_collation(0x00000409, 52);
        // B_VARCHAR("") + collation(6) + B_VARCHAR("")
        assert_eq!(data.initial_block_length(), 1 + 6 + 1);
    }

    #[test]
    fn initial_block_length_with_state_entries() {
        let mut data = make_recovery_data_empty();
        // Small state: id(1) + len(1) + data(3) = 5
        data.initial_state[0] = Some(SessionStateRecord {
            recoverable: true,
            sequence: 1,
            data: vec![0x01, 0x02, 0x03],
        });
        // Another entry: id(1) + len(1) + data(1) = 3
        data.initial_state[5] = Some(SessionStateRecord {
            recoverable: true,
            sequence: 1,
            data: vec![0xAA],
        });
        // base (3) + 5 + 3
        assert_eq!(data.initial_block_length(), 3 + 5 + 3);
    }

    #[test]
    fn initial_block_length_large_state_entry() {
        let mut data = make_recovery_data_empty();
        // 256 bytes of data → id(1) + marker(1) + dword(4) + data(256) = 262
        data.initial_state[10] = Some(SessionStateRecord {
            recoverable: true,
            sequence: 1,
            data: vec![0xBB; 256],
        });
        // base (3) + 262
        assert_eq!(data.initial_block_length(), 3 + 262);
    }

    #[test]
    fn delta_block_length_all_same_as_initial() {
        let data = make_recovery_data_empty();
        // database same → 1, collation same → 1, language same → 1
        assert_eq!(data.delta_block_length(), 3);
    }

    #[test]
    fn delta_block_length_database_changed() {
        let mut data = make_recovery_data_empty();
        data.initial_database = "master".to_string();
        data.database = "tempdb".to_string(); // 6 chars → 1 + 12 = 13
        data.initial_language = "".to_string();
        data.language = "".to_string();
        // database(13) + collation same(1) + language same(1)
        assert_eq!(data.delta_block_length(), 13 + 1 + 1);
    }

    #[test]
    fn delta_block_length_collation_changed() {
        let mut data = make_recovery_data_empty();
        data.collation = make_collation(0x00000409, 52);
        // database same(1) + collation changed(6) + language same(1)
        assert_eq!(data.delta_block_length(), 1 + 6 + 1);
    }

    #[test]
    fn delta_block_length_all_changed() {
        let mut data = make_recovery_data_empty();
        data.initial_database = "master".to_string();
        data.database = "mydb".to_string(); // 4 chars → 1 + 8 = 9
        data.initial_language = "us_english".to_string();
        data.language = "Deutsch".to_string(); // 7 chars → 1 + 14 = 15
        data.collation = make_collation(0x00000409, 52);
        assert_eq!(data.delta_block_length(), 9 + 6 + 15);
    }

    #[test]
    fn delta_block_length_with_state_entries() {
        let mut data = make_recovery_data_empty();
        data.delta[3] = Some(SessionStateRecord {
            recoverable: true,
            sequence: 2,
            data: vec![0x01, 0x02],
        });
        // base(3) + id(1) + len(1) + data(2) = 7
        assert_eq!(data.delta_block_length(), 3 + 4);
    }

    #[test]
    fn total_data_length_empty() {
        let data = make_recovery_data_empty();
        // 8 (two DWORDs) + initial(3) + delta(3) = 14
        assert_eq!(data.total_data_length(), 14);
    }

    #[test]
    fn data_length_reconnection_matches_total() {
        let mut data = make_recovery_data_empty();
        data.initial_database = "master".to_string();
        data.initial_language = "us_english".to_string();
        data.database = "master".to_string();
        data.language = "us_english".to_string();
        let feature = SessionRecoveryFeature::new_for_reconnection(Box::new(data.clone()));
        // data_length() = 1 (feat_id) + 4 (total_data_len) + 4 (initial_len) + initial + 4 (delta_len) + delta
        let expected = 1 + 4 + 4 + data.initial_block_length() + 4 + data.delta_block_length();
        assert_eq!(feature.data_length(), expected as i32);
    }

    #[test]
    fn serialize_initial_login_writes_five_bytes() {
        use crate::io::packet_writer::tests::MockNetworkWriter;
        use crate::message::messages::PacketType;
        use futures::executor::block_on;

        let mut mock_writer = MockNetworkWriter::new(4096);
        let mut pw = PacketWriter::new(PacketType::Login7, &mut mock_writer, None, None);

        let feature = SessionRecoveryFeature::new(1);
        block_on(feature.serialize(&mut pw)).unwrap();

        let payload = pw.get_payload();
        let bytes = payload.get_ref();
        // Skip 8-byte packet header
        let feature_bytes = &bytes[8..];
        assert_eq!(feature_bytes[0], 0x01); // feature ID
        assert_eq!(&feature_bytes[1..5], &[0, 0, 0, 0]); // data_length = 0
    }

    #[test]
    fn serialize_reconnection_empty_state() {
        use crate::io::packet_writer::tests::MockNetworkWriter;
        use crate::message::messages::PacketType;
        use futures::executor::block_on;

        let data = make_recovery_data_empty();
        let feature = SessionRecoveryFeature::new_for_reconnection(Box::new(data));

        let mut mock_writer = MockNetworkWriter::new(4096);
        let mut pw = PacketWriter::new(PacketType::Login7, &mut mock_writer, None, None);
        block_on(feature.serialize(&mut pw)).unwrap();

        let payload = pw.get_payload();
        let bytes = payload.get_ref();
        let fb = &bytes[8..]; // skip packet header

        assert_eq!(fb[0], 0x01); // feature ID = SRecovery
        // total_data_length = 14 (8 + 3 + 3)
        assert_eq!(u32::from_le_bytes([fb[1], fb[2], fb[3], fb[4]]), 14);
        // initial_length = 3
        assert_eq!(u32::from_le_bytes([fb[5], fb[6], fb[7], fb[8]]), 3);
        // initial block: db(0) + collation(0) + lang(0)
        assert_eq!(fb[9], 0); // empty database
        assert_eq!(fb[10], 0); // default collation (length=0)
        assert_eq!(fb[11], 0); // empty language
        // current_length = 3
        assert_eq!(u32::from_le_bytes([fb[12], fb[13], fb[14], fb[15]]), 3);
        // delta block: db same(0) + collation same(0) + lang same(0)
        assert_eq!(fb[16], 0);
        assert_eq!(fb[17], 0);
        assert_eq!(fb[18], 0);
    }

    #[test]
    fn serialize_reconnection_with_database_and_state() {
        use crate::io::packet_writer::tests::MockNetworkWriter;
        use crate::message::messages::PacketType;
        use futures::executor::block_on;

        let mut data = make_recovery_data_empty();
        data.initial_database = "AB".to_string(); // 2 UTF-16 chars
        data.database = "AB".to_string(); // same as initial
        data.initial_state[0] = Some(SessionStateRecord {
            recoverable: true,
            sequence: 1,
            data: vec![0xDE, 0xAD],
        });

        let feature = SessionRecoveryFeature::new_for_reconnection(Box::new(data));

        let mut mock_writer = MockNetworkWriter::new(4096);
        let mut pw = PacketWriter::new(PacketType::Login7, &mut mock_writer, None, None);
        block_on(feature.serialize(&mut pw)).unwrap();

        let payload = pw.get_payload();
        let bytes = payload.get_ref();
        let fb = &bytes[8..];

        assert_eq!(fb[0], 0x01); // feature ID

        // initial_block_length: B_VARCHAR("AB") = 1+4=5, collation=1, lang=1, state=1+1+2=4 → 11
        let initial_len = u32::from_le_bytes([fb[5], fb[6], fb[7], fb[8]]);
        assert_eq!(initial_len, 11);

        // total = 8 + 11 + 3
        let total = u32::from_le_bytes([fb[1], fb[2], fb[3], fb[4]]);
        assert_eq!(total, 22);

        // Initial block starts at offset 9:
        // B_VARCHAR("AB"): length=2, then 'A'=0x41,0x00 'B'=0x42,0x00
        assert_eq!(fb[9], 2); // char count
        assert_eq!(fb[10], 0x41);
        assert_eq!(fb[11], 0x00); // 'A' UTF-16LE
        assert_eq!(fb[12], 0x42);
        assert_eq!(fb[13], 0x00); // 'B' UTF-16LE
        // Collation: default → 0
        assert_eq!(fb[14], 0);
        // Language: empty → 0
        assert_eq!(fb[15], 0);
        // State entry 0: state_id=0, len=2, data=[0xDE, 0xAD]
        assert_eq!(fb[16], 0); // state_id
        assert_eq!(fb[17], 2); // length
        assert_eq!(fb[18], 0xDE); // data[0]
        assert_eq!(fb[19], 0xAD); // data[1]

        // Delta block at offset 20:
        let delta_len = u32::from_le_bytes([fb[20], fb[21], fb[22], fb[23]]);
        assert_eq!(delta_len, 3); // all same, no delta entries
        assert_eq!(fb[24], 0); // db same
        assert_eq!(fb[25], 0); // collation same
        assert_eq!(fb[26], 0); // language same
    }

    #[test]
    fn serialize_reconnection_with_changed_database() {
        use crate::io::packet_writer::tests::MockNetworkWriter;
        use crate::message::messages::PacketType;
        use futures::executor::block_on;

        let mut data = make_recovery_data_empty();
        data.initial_database = "A".to_string();
        data.database = "B".to_string(); // changed

        let feature = SessionRecoveryFeature::new_for_reconnection(Box::new(data));

        let mut mock_writer = MockNetworkWriter::new(4096);
        let mut pw = PacketWriter::new(PacketType::Login7, &mut mock_writer, None, None);
        block_on(feature.serialize(&mut pw)).unwrap();

        let payload = pw.get_payload();
        let bytes = payload.get_ref();
        let fb = &bytes[8..];

        // initial_block: B_VARCHAR("A")=1+2=3, collation=1, lang=1 → 5
        let initial_len = u32::from_le_bytes([fb[5], fb[6], fb[7], fb[8]]);
        assert_eq!(initial_len, 5);

        // delta_block: B_VARCHAR("B")=1+2=3, collation same=1, lang same=1 → 5
        // Find delta block start = 9 + 5 = 14 (after initial block)
        let delta_start = 9 + initial_len as usize;
        let delta_len = u32::from_le_bytes([
            fb[delta_start],
            fb[delta_start + 1],
            fb[delta_start + 2],
            fb[delta_start + 3],
        ]);
        assert_eq!(delta_len, 5);

        // Delta database: B_VARCHAR("B") = char_count=1, then 'B'=0x42,0x00
        let db_offset = delta_start + 4;
        assert_eq!(fb[db_offset], 1); // char count
        assert_eq!(fb[db_offset + 1], 0x42);
        assert_eq!(fb[db_offset + 2], 0x00);
    }

    #[test]
    fn serialize_reconnection_with_collation() {
        use crate::io::packet_writer::tests::MockNetworkWriter;
        use crate::message::messages::PacketType;
        use futures::executor::block_on;

        let coll = make_collation(0x00000409, 52);
        let mut data = make_recovery_data_empty();
        data.initial_collation = coll;
        data.collation = coll; // same

        let feature = SessionRecoveryFeature::new_for_reconnection(Box::new(data));

        let mut mock_writer = MockNetworkWriter::new(4096);
        let mut pw = PacketWriter::new(PacketType::Login7, &mut mock_writer, None, None);
        block_on(feature.serialize(&mut pw)).unwrap();

        let payload = pw.get_payload();
        let bytes = payload.get_ref();
        let fb = &bytes[8..];

        // initial_block: db(1) + collation(6) + lang(1) = 8
        let initial_len = u32::from_le_bytes([fb[5], fb[6], fb[7], fb[8]]);
        assert_eq!(initial_len, 8);

        // Collation at offset 9+1=10: length=5, then 4 bytes info + 1 byte sort_id
        assert_eq!(fb[10], 5); // collation length
        let info_bytes = u32::from_le_bytes([fb[11], fb[12], fb[13], fb[14]]);
        assert_eq!(info_bytes, 0x00000409);
        assert_eq!(fb[15], 52); // sort_id
    }

    #[test]
    fn data_length_equals_serialized_bytes() {
        use crate::io::packet_writer::tests::MockNetworkWriter;
        use crate::message::messages::PacketType;
        use futures::executor::block_on;

        let mut data = make_recovery_data_empty();
        data.initial_database = "master".to_string();
        data.initial_language = "us_english".to_string();
        data.initial_collation = make_collation(0x00000409, 52);
        data.database = "tempdb".to_string();
        data.language = "us_english".to_string();
        data.collation = make_collation(0x00000409, 52);
        data.initial_state[1] = Some(SessionStateRecord {
            recoverable: true,
            sequence: 1,
            data: vec![0x01, 0x02, 0x03],
        });
        data.delta[2] = Some(SessionStateRecord {
            recoverable: true,
            sequence: 2,
            data: vec![0xFF; 10],
        });

        let feature = SessionRecoveryFeature::new_for_reconnection(Box::new(data));
        let expected_len = feature.data_length();

        let mut mock_writer = MockNetworkWriter::new(4096);
        let mut pw = PacketWriter::new(PacketType::Login7, &mut mock_writer, None, None);
        block_on(feature.serialize(&mut pw)).unwrap();

        let payload = pw.get_payload();
        let actual_len = payload.position() as i32 - 8; // subtract packet header
        assert_eq!(actual_len, expected_len);
    }
}
