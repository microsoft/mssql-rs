// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::core::{EncryptionSetting, TdsResult};
use crate::io::packet_reader::TdsPacketReader;
use crate::message::messages::{PacketType, Request};
use crate::{
    core::{SQLServerVersion, Version},
    io::packet_writer::{PacketWriter, TdsPacketWriter},
};
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering::Relaxed;
use std::thread;
use std::thread::ThreadId;
use uuid::Uuid;

#[derive(PartialEq)]
pub enum EncryptionType {
    Off,
    On,
    NotSupported,
    Required,
    Unknown(u8),
}

impl std::convert::From<u8> for EncryptionType {
    fn from(v: u8) -> Self {
        match v {
            0x00 => EncryptionType::Off,
            0x01 => EncryptionType::On,
            0x02 => EncryptionType::NotSupported,
            0x03 => EncryptionType::Required,
            _ => EncryptionType::Unknown(v),
        }
    }
}

impl EncryptionType {
    pub fn to_u8(&self) -> u8 {
        match self {
            EncryptionType::Off => 0x00,
            EncryptionType::On => 0x01,
            EncryptionType::NotSupported => 0x02,
            EncryptionType::Required => 0x03,
            EncryptionType::Unknown(v) => *v,
        }
    }
}

pub enum FederationType {
    Off = 0x00,
    On = 0x01,
}

pub enum MarsType {
    Off = 0x00,
    On = 0x01,
}

pub enum OptionType {
    Version,
    Encryption,
    InstOpt,
    ThreadId,
    Mars,
    TraceId,
    FedAuthRequired,
    Nonce,
    Terminator,
    Unknown(u8),
}

impl std::convert::From<u8> for OptionType {
    fn from(v: u8) -> Self {
        match v {
            0x00 => OptionType::Version,
            0x01 => OptionType::Encryption,
            0x02 => OptionType::InstOpt,
            0x03 => OptionType::ThreadId,
            0x04 => OptionType::Mars,
            0x05 => OptionType::TraceId,
            0x06 => OptionType::FedAuthRequired,
            0x07 => OptionType::Nonce,
            0xff => OptionType::Terminator,
            _ => OptionType::Unknown(v),
        }
    }
}

impl OptionType {
    pub fn to_u8(&self) -> u8 {
        match self {
            OptionType::Version => 0x00,
            OptionType::Encryption => 0x01,
            OptionType::InstOpt => 0x02,
            OptionType::ThreadId => 0x03,
            OptionType::Mars => 0x04,
            OptionType::TraceId => 0x05,
            OptionType::FedAuthRequired => 0x06,
            OptionType::Nonce => 0x07,
            OptionType::Terminator => 0xff,
            OptionType::Unknown(v) => *v,
        }
    }
}

pub struct PreloginRequestModel {
    pub sdk_version: Version,
    pub connection_id: Uuid,
    pub activity_id: Uuid,
    pub activity_sequence_number: i32,
    pub mars_enabled: bool,
    #[allow(dead_code)]
    pub thread_id: ThreadId,
    pub encryption_setting: EncryptionSetting,
    pub database_instance: String,
    pub fed_auth: bool,
}

static REQUEST_COUNT: AtomicI32 = AtomicI32::new(0);
impl PreloginRequestModel {
    pub fn new(
        connection_id: Uuid,
        mars_enabled: Option<bool>,
        encryption_setting: Option<EncryptionSetting>,
        database_instance: Option<&str>,
    ) -> Self {
        let mars_enabled = mars_enabled.unwrap_or(false);
        let encryption_setting = encryption_setting.unwrap_or(EncryptionSetting::Strict);
        let database_instance = database_instance.unwrap_or("MSSQLServer").to_string();
        PreloginRequestModel {
            sdk_version: Version {
                major: 0,
                minor: 0,
                build: 0,
                revision: 1,
            },
            connection_id,
            activity_id: Uuid::new_v4(),
            activity_sequence_number: REQUEST_COUNT.fetch_add(1, Relaxed),
            mars_enabled,
            thread_id: thread::current().id(),
            encryption_setting,
            database_instance: database_instance.to_string(),
            fed_auth: true, // This is forced to true to allow detecting if federated auth is supported.
        }
    }
}

pub struct PreloginResponseModel {
    pub encryption: EncryptionType,
    pub federated_auth_supported: bool,
    pub dbinstance_valid: bool,
    pub mars_enabled: bool,
    pub server_version: Version,
    pub sql_server_version: SQLServerVersion,
}

impl PreloginResponseModel {
    fn new() -> Self {
        PreloginResponseModel {
            server_version: Version::new(0, 0, 0, 0),
            sql_server_version: SQLServerVersion::SqlServerNotsupported,
            encryption: EncryptionType::Off,
            dbinstance_valid: false,
            mars_enabled: false,
            federated_auth_supported: false,
        }
    }
}

pub struct PreloginRequest<'model> {
    pub model: &'model PreloginRequestModel,
}

#[async_trait]
impl Request for PreloginRequest<'_> {
    fn packet_type(&self) -> PacketType {
        PacketType::PreLogin
    }

    async fn serialize<'a, 'b>(&'a self, writer: &'a mut PacketWriter<'b>) -> TdsResult<()>
    where
        'b: 'a,
    {
        let mut serializer = Serializer::new(self.model, writer);
        serializer.serialize().await?;
        Ok(())
    }
}

pub struct PreloginResponse {}

impl PreloginResponse {
    pub(crate) async fn deserialize<T: TdsPacketReader>(
        &self,
        packet_reader: &mut T,
    ) -> TdsResult<PreloginResponseModel> {
        struct OptionContext {
            option: OptionType,
            length: usize,
        }
        let mut contexts = VecDeque::new();
        packet_reader.reset_reader();
        loop {
            let token = packet_reader.read_byte().await?;
            if token == 0xFF {
                break;
            }

            let option = OptionType::from(token);
            let _ = packet_reader.read_int16_big_endian().await?; // offset.
            let length = packet_reader.read_int16_big_endian().await?;

            // Record the length and option type for later deserialization of the value.
            contexts.push_back(OptionContext {
                option,
                length: length as usize,
            });
        }

        let mut result = PreloginResponseModel::new();
        for context in contexts {
            if context.length == 0 {
                continue;
            }

            match context.option {
                OptionType::Version => {
                    let major = packet_reader.read_byte().await?;
                    let minor = packet_reader.read_byte().await?;
                    let build = packet_reader.read_int16_big_endian().await?;
                    let revision = packet_reader.read_int16_big_endian().await?;
                    result.server_version =
                        Version::new(major, minor, build as u16, revision as u16);
                    result.sql_server_version = SQLServerVersion::from(major);
                }
                OptionType::Encryption => {
                    result.encryption = EncryptionType::from(packet_reader.read_byte().await?);
                    // encryption type.
                }
                OptionType::InstOpt => {
                    result.dbinstance_valid = packet_reader.read_byte().await? == 0;
                }
                OptionType::Mars => {
                    result.mars_enabled = packet_reader.read_byte().await? == 1;
                }
                OptionType::FedAuthRequired => {
                    result.federated_auth_supported = packet_reader.read_byte().await? == 1;
                }
                _ => {
                    // Todo: Logging that this is being skipped.
                    packet_reader.skip_bytes(context.length).await?;
                }
            };
        }
        Ok(result)
    }
}

struct Serializer<'a, 'n> {
    model: &'a PreloginRequestModel,
    payload_writer: &'a mut PacketWriter<'n>,
    content_next_offset: u32,
    instance_bytes: &'a [u8],
}

impl<'a, 'n> Serializer<'a, 'n> {
    pub fn new(
        model: &'a PreloginRequestModel,
        payload_writer: &'a mut PacketWriter<'n>,
    ) -> Serializer<'a, 'n> {
        Serializer {
            model,
            payload_writer,
            // In total there are 7 DataModel (Version, Encryption, Instance, ThreadID, MARS, FEDAUTH, TRACEID) + Terminator
            // Each option has a size of 5 bytes (1 byte for TokenType, 2 bytes for offset, 2 bytes for length)
            // so content section will start at 35 + 1 (terminator byte) = 36
            // The content section hence starts from position 45.but the offset is 0 based so we start from 44
            content_next_offset: 36,
            instance_bytes: model.database_instance.as_bytes(),
        }
    }
    async fn serialize(&mut self) -> TdsResult<()> {
        // Write headers then terminate the header table.
        self.write_headers().await?;
        self.write_terminator().await?;

        // Write data values. Must be the same order was what's in write_headers.
        self.write_version().await?;
        self.write_encryption().await?;
        self.write_inst_opt().await?;
        self.write_thread_id().await?;
        self.write_mars().await?;
        self.write_trace_id().await?;
        self.write_fed_auth_required().await?;

        self.payload_writer.finalize().await?;
        Ok(())
    }

    async fn write_headers(&mut self) -> TdsResult<()> {
        self.write_option_metadata(OptionType::Version, 6).await?;
        self.write_option_metadata(OptionType::Encryption, 1)
            .await?;
        self.write_option_metadata(OptionType::InstOpt, (self.instance_bytes.len() + 1) as u16)
            .await?;
        self.write_option_metadata(OptionType::ThreadId, 4).await?;
        self.write_option_metadata(OptionType::Mars, 1).await?;

        let length = (16 + 16 + 4) as u16; // two GUIDs and one 32-bit integer.
        self.write_option_metadata(OptionType::TraceId, length)
            .await?;
        self.write_option_metadata(OptionType::FedAuthRequired, 1)
            .await?;
        Ok(())
    }

    async fn write_version(&mut self) -> TdsResult<()> {
        self.payload_writer
            .write_byte_async(self.model.sdk_version.major)
            .await?;
        self.payload_writer
            .write_byte_async(self.model.sdk_version.minor)
            .await?;
        self.payload_writer
            .write_i16_be_async(self.model.sdk_version.build as i16)
            .await?;
        self.payload_writer
            .write_i16_be_async(self.model.sdk_version.revision as i16)
            .await?;
        Ok(())
    }

    async fn write_encryption(&mut self) -> TdsResult<()> {
        match self.model.encryption_setting {
            EncryptionSetting::PreferOff => {
                self.payload_writer
                    .write_byte_async(EncryptionType::Off.to_u8())
                    .await
            }
            EncryptionSetting::On => {
                self.payload_writer
                    .write_byte_async(EncryptionType::On.to_u8())
                    .await
            }
            EncryptionSetting::Required => {
                self.payload_writer
                    .write_byte_async(EncryptionType::Required.to_u8())
                    .await
            }
            _ => {
                // This includes Strict, which the server ignores because it is always on in TDS 8.
                self.payload_writer
                    .write_byte_async(EncryptionType::NotSupported.to_u8())
                    .await
            }
        }
    }

    async fn write_inst_opt(&mut self) -> TdsResult<()> {
        self.payload_writer.write_async(self.instance_bytes).await?;
        self.payload_writer.write_byte_async(0).await?;
        Ok(())
    }

    async fn write_thread_id(&mut self) -> TdsResult<()> {
        // Revisit because Rust's ThreadId is not the same numerically as the OS-level thread id.
        self.payload_writer.write_i32_be_async(0).await?;
        Ok(())
    }

    async fn write_mars(&mut self) -> TdsResult<()> {
        self.payload_writer
            .write_byte_async(match self.model.mars_enabled {
                true => MarsType::On as u8,
                false => MarsType::Off as u8,
            })
            .await?;
        Ok(())
    }

    async fn write_trace_id(&mut self) -> TdsResult<()> {
        let activity_id_bytes = self.model.activity_id.as_bytes();
        let connection_id_bytes = self.model.connection_id.as_bytes();
        self.payload_writer.write_async(activity_id_bytes).await?;
        self.payload_writer.write_async(connection_id_bytes).await?;
        self.payload_writer
            .write_i32_async(self.model.activity_sequence_number)
            .await?;
        Ok(())
    }

    async fn write_fed_auth_required(&mut self) -> TdsResult<()> {
        self.payload_writer
            .write_byte_async(match self.model.fed_auth {
                true => FederationType::On as u8,
                false => FederationType::Off as u8,
            })
            .await?;
        Ok(())
    }

    async fn write_terminator(&mut self) -> TdsResult<()> {
        self.payload_writer
            .write_byte_async(OptionType::Terminator.to_u8())
            .await?;
        self.content_next_offset += 1;
        Ok(())
    }

    async fn write_option_metadata(&mut self, option: OptionType, length: u16) -> TdsResult<()> {
        self.payload_writer.write_byte_async(option.to_u8()).await?;
        self.payload_writer
            .write_i16_be_async(self.content_next_offset as i16)
            .await?;
        self.payload_writer
            .write_i16_be_async(length as i16)
            .await?;
        self.content_next_offset += length as u32;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::vec;

    use crate::core::{EncryptionSetting, SQLServerVersion, Version};
    use crate::message::messages::PacketType;
    use crate::message::prelogin::{
        OptionType, PreloginRequestModel, PreloginResponse, Serializer,
    };

    use crate::io::packet_reader::TdsPacketReader;
    use crate::io::packet_writer::PacketWriter;
    use crate::io::packet_writer::tests::MockNetworkWriter;
    use async_trait::async_trait;
    use byteorder::{BigEndian, ReadBytesExt};
    use futures::executor::block_on;

    use crate::core::TdsResult;
    use uuid::Uuid;

    mockall::mock! {
        pub TestPacketReader {}
        #[async_trait]
        impl TdsPacketReader for TestPacketReader {
            async fn read_byte(&mut self) -> TdsResult<u8>;
            async fn read_int16_big_endian(&mut self) -> TdsResult<i16>;
            async fn read_int32_big_endian(&mut self) -> TdsResult<i32>;
            async fn read_uint40(&mut self) -> TdsResult<u64>;

            async fn read_float32(&mut self) -> TdsResult<f32>;
            async fn read_float64(&mut self) -> TdsResult<f64>;
            async fn read_int16(&mut self) -> TdsResult<i16>;
            async fn read_uint16(&mut self) -> TdsResult<u16>;
            async fn read_uint24(&mut self) -> TdsResult<u32>;
            async fn read_int32(&mut self) -> TdsResult<i32>;
            async fn read_uint32(&mut self) -> TdsResult<u32>;
            async fn read_int64(&mut self) -> TdsResult<i64>;
            async fn read_uint64(&mut self) -> TdsResult<u64>;

            async fn read_bytes(&mut self, buffer: &mut [u8]) -> TdsResult<usize>;
            async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>>;
            async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>>;
            async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>>;
            async fn read_varchar_u8_length(&mut self) -> TdsResult<String>;
            async fn read_unicode(&mut self, string_length: usize) -> TdsResult<String>;
            async fn read_unicode_with_byte_length(&mut self, byte_length: usize) -> TdsResult<String>;
            async fn skip_bytes(&mut self, skip_count: usize) -> TdsResult<()>;
            async fn cancel_read_stream(&mut self) -> TdsResult<()>;
            fn reset_reader(&mut self);
        }
    }

    #[test]
    fn test_default_model() {
        let model = PreloginRequestModel::new(
            Uuid::new_v4(),
            Option::from(false),
            Option::from(EncryptionSetting::Required),
            Option::from("MSSQLServer"),
        );
        let mut mock = MockNetworkWriter::new(1024);
        let mut packet_writer = PacketWriter::new(PacketType::PreLogin, &mut mock, None, None);

        let mut serializer = Serializer::new(&model, &mut packet_writer);
        block_on(serializer.serialize()).unwrap();

        let mut cursor = packet_writer.get_payload();

        // Just validate that the version was serialized correctly to start with.

        // Validate a few headers.
        assert_eq!(cursor.read_u8().unwrap(), OptionType::Version.to_u8());
        assert_eq!(cursor.read_i16::<BigEndian>().unwrap(), 36); // Initial content_next_offset.
        assert_eq!(cursor.read_i16::<BigEndian>().unwrap(), 6);

        assert_eq!(cursor.read_u8().unwrap(), OptionType::Encryption.to_u8());
        assert_eq!(cursor.read_i16::<BigEndian>().unwrap(), 42); // Add the length of the previous header to the content_next_offset.
        assert_eq!(cursor.read_i16::<BigEndian>().unwrap(), 1);
    }

    #[test]
    fn test_deserialize_model() {
        let response = PreloginResponse {};
        let mut mocked_packet_reader = MockTestPacketReader::new();
        let byte_data = vec![
            OptionType::Version.to_u8(), // OptionType::Version
            OptionType::Mars.to_u8(),
            0xFF, // terminator
            15,
            2, // Version
            1, // Mars enabled
        ];

        let i16_data: Vec<i16> = vec![0, 6, 0, 6, 3, 4]; // Two i16 values for offsets and lengths.

        // let mut seq = Sequence::new();

        // First expect reset_reader to be called
        mocked_packet_reader
            .expect_reset_reader()
            .once()
            // .in_sequence(&mut seq)
            .returning(|| {});

        for &b in &byte_data {
            mocked_packet_reader
                .expect_read_byte()
                .once()
                .returning(move || Ok(b));
        }

        for &b in &i16_data {
            mocked_packet_reader
                .expect_read_int16_big_endian()
                .once()
                .returning(move || Ok(b));
        }

        let response_model = block_on(response.deserialize(&mut mocked_packet_reader)).unwrap();

        // Compare the guid, which is auto-generated.
        assert!(response_model.mars_enabled);
        assert_eq!(
            response_model.sql_server_version,
            SQLServerVersion::SqlServer2019
        );
        assert_eq!(response_model.server_version, Version::new(15, 2, 3, 4));
    }
}
