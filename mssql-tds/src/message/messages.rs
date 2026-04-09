// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::core::{CancelHandle, NegotiatedEncryptionSetting, TdsResult};
use crate::{
    io::{packet_writer::PacketWriter, reader_writer::NetworkWriter},
    token::tokens::ErrorToken,
};
use async_trait::async_trait;

#[derive(Copy, Clone)]
#[allow(dead_code, clippy::upper_case_acronyms)]
pub enum PacketType {
    Unknown = 0x00,
    SqlBatch = 0x01,
    RpcRequest = 0x03,
    TabularResult = 0x04,
    Attention = 0x06,
    BulkLoad = 0x07,
    FedAuthToken = 0x08,
    TransactionManager = 0x0E,
    Login7 = 0x10,
    SSPI = 0x11,
    PreLogin = 0x12,
}

impl<'a, 'b> PacketType
where
    'a: 'b,
{
    pub(crate) fn create_packet_writer(
        &self,
        transport: &'a mut dyn NetworkWriter,
        timeout: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> PacketWriter<'a> {
        PacketWriter::new(*self, transport, timeout, cancel_handle)
    }

    pub(crate) async fn first_packet_callback(
        &self,
        writer: &'b mut dyn NetworkWriter,
    ) -> TdsResult<()> {
        match self {
            PacketType::Login7 => {
                if writer.get_encryption_setting() == NegotiatedEncryptionSetting::LoginOnly {
                    // Only the first packet should be encrypted. Turn off encryption after the first packet.
                    writer.disable_ssl().await
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }
}

/// Represents the status flags for a packet.
#[repr(u8)]
pub(crate) enum PacketStatusFlags {
    /// Normal Packet.
    Normal = 0x00,

    /// End of Message. The last packet in the message.
    Eom = 0x01,

    /// Packet/Message to be ignored.
    Ignore = 0x02,

    #[allow(dead_code)] // Not used currently.
    /// Reset connection.
    ResetConnection = 0x08,

    #[allow(dead_code)] // Not used currently.
    /// Reset connection but keep transaction state.
    ResetConnectionSkipTran = 0x10,
}

#[async_trait]
pub(crate) trait Request {
    fn packet_type(&self) -> PacketType;

    fn create_packet_writer<'a>(
        &self,
        writer: &'a mut dyn NetworkWriter,
        timeout: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> PacketWriter<'a> {
        self.packet_type()
            .create_packet_writer(writer, timeout, cancel_handle)
    }

    async fn serialize<'a, 'b>(&'a self, writer: &'a mut PacketWriter<'b>) -> TdsResult<()>
    where
        'b: 'a;
}

pub(crate) struct TdsError {
    pub(crate) error_token: ErrorToken,
}

impl TdsError {
    pub fn new(error_token: ErrorToken) -> Self {
        TdsError { error_token }
    }

    pub fn get_message(&self) -> String {
        self.error_token.message.clone()
    }
}

#[allow(dead_code)]
pub struct TdsInfo {}

#[allow(dead_code)]
pub struct TokenResponse {}

impl TokenResponse {
    // TODO:
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packet_type_discriminants() {
        assert_eq!(PacketType::Unknown as u8, 0x00);
        assert_eq!(PacketType::SqlBatch as u8, 0x01);
        assert_eq!(PacketType::RpcRequest as u8, 0x03);
        assert_eq!(PacketType::TabularResult as u8, 0x04);
        assert_eq!(PacketType::Attention as u8, 0x06);
        assert_eq!(PacketType::BulkLoad as u8, 0x07);
        assert_eq!(PacketType::FedAuthToken as u8, 0x08);
        assert_eq!(PacketType::TransactionManager as u8, 0x0E);
        assert_eq!(PacketType::Login7 as u8, 0x10);
        assert_eq!(PacketType::SSPI as u8, 0x11);
        assert_eq!(PacketType::PreLogin as u8, 0x12);
    }

    #[test]
    fn test_packet_status_flags_values() {
        assert_eq!(PacketStatusFlags::Normal as u8, 0x00);
        assert_eq!(PacketStatusFlags::Eom as u8, 0x01);
        assert_eq!(PacketStatusFlags::Ignore as u8, 0x02);
        assert_eq!(PacketStatusFlags::ResetConnection as u8, 0x08);
        assert_eq!(PacketStatusFlags::ResetConnectionSkipTran as u8, 0x10);
    }

    #[test]
    fn test_packet_status_flags_bitmask_combine() {
        let combined = PacketStatusFlags::Eom as u8 | PacketStatusFlags::Ignore as u8;
        assert_eq!(combined, 0x03);
    }

    #[test]
    fn test_tds_error_new_and_get_message() {
        let token = ErrorToken {
            number: 208,
            state: 1,
            severity: 16,
            message: "Invalid object name 'foo'".to_string(),
            server_name: "localhost".to_string(),
            proc_name: String::new(),
            line_number: 1,
        };
        let error = TdsError::new(token);
        assert_eq!(error.get_message(), "Invalid object name 'foo'");
        assert_eq!(error.error_token.number, 208);
        assert_eq!(error.error_token.severity, 16);
    }

    #[test]
    fn test_tds_error_empty_message() {
        let token = ErrorToken {
            number: 0,
            state: 0,
            severity: 0,
            message: String::new(),
            server_name: String::new(),
            proc_name: String::new(),
            line_number: 0,
        };
        let error = TdsError::new(token);
        assert_eq!(error.get_message(), "");
    }

    #[test]
    fn test_packet_type_is_copy() {
        let pt = PacketType::SqlBatch;
        let pt2 = pt;
        assert_eq!(pt as u8, pt2 as u8);
    }

    // Mock writer for first_packet_callback tests
    struct MockWriter {
        encryption: NegotiatedEncryptionSetting,
        ssl_disabled: bool,
    }

    impl MockWriter {
        fn new(encryption: NegotiatedEncryptionSetting) -> Self {
            Self {
                encryption,
                ssl_disabled: false,
            }
        }
    }

    #[async_trait]
    impl crate::connection::transport::network_transport::TransportSslHandler for MockWriter {
        async fn enable_ssl(&mut self) -> TdsResult<()> {
            Ok(())
        }
        async fn disable_ssl(&mut self) -> TdsResult<()> {
            self.ssl_disabled = true;
            Ok(())
        }
    }

    #[async_trait]
    impl crate::io::reader_writer::NetworkWriter for MockWriter {
        async fn send(&mut self, _data: &[u8]) -> TdsResult<()> {
            Ok(())
        }
        fn packet_size(&self) -> u32 {
            4096
        }
        fn get_encryption_setting(&self) -> NegotiatedEncryptionSetting {
            self.encryption
        }
    }

    #[tokio::test]
    async fn test_first_packet_callback_login7_login_only_disables_ssl() {
        let mut writer = MockWriter::new(NegotiatedEncryptionSetting::LoginOnly);
        PacketType::Login7
            .first_packet_callback(&mut writer)
            .await
            .unwrap();
        assert!(writer.ssl_disabled);
    }

    #[tokio::test]
    async fn test_first_packet_callback_login7_mandatory_no_disable() {
        let mut writer = MockWriter::new(NegotiatedEncryptionSetting::Mandatory);
        PacketType::Login7
            .first_packet_callback(&mut writer)
            .await
            .unwrap();
        assert!(!writer.ssl_disabled);
    }

    #[tokio::test]
    async fn test_first_packet_callback_non_login7_noop() {
        let mut writer = MockWriter::new(NegotiatedEncryptionSetting::LoginOnly);
        PacketType::SqlBatch
            .first_packet_callback(&mut writer)
            .await
            .unwrap();
        assert!(!writer.ssl_disabled);
    }
}
