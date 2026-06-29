// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TDS Protocol implementation for the mock server

use bytes::{Buf, BufMut, BytesMut};
use std::io;
use thiserror::Error;
use tracing::{debug, trace};

/// TDS Packet Header size (8 bytes)
pub const PACKET_HEADER_SIZE: usize = 8;

/// Maximum packet size
pub const MAX_PACKET_SIZE: usize = 4096;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("Invalid packet type: {0}")]
    InvalidPacketType(u8),

    #[error("Invalid packet size: {0}")]
    InvalidPacketSize(usize),

    #[error("Protocol error: {0}")]
    Protocol(String),
}

/// TDS Packet Types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketType {
    SqlBatch = 0x01,
    FedAuthToken = 0x08,
    PreLogin = 0x12,
    TabularResult = 0x04,
    Attention = 0x06,
    Login7 = 0x10,
    RpcRequest = 0x03,
}

impl TryFrom<u8> for PacketType {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(PacketType::SqlBatch),
            0x08 => Ok(PacketType::FedAuthToken),
            0x12 => Ok(PacketType::PreLogin),
            0x04 => Ok(PacketType::TabularResult),
            0x06 => Ok(PacketType::Attention),
            0x10 => Ok(PacketType::Login7),
            0x03 => Ok(PacketType::RpcRequest),
            _ => Err(ProtocolError::InvalidPacketType(value)),
        }
    }
}

/// TDS Packet Status
#[derive(Debug, Clone, Copy)]
pub struct PacketStatus(u8);

impl PacketStatus {
    pub fn normal() -> Self {
        Self(0x00)
    }

    pub fn end_of_message() -> Self {
        Self(0x01)
    }

    pub fn is_end_of_message(&self) -> bool {
        self.0 & 0x01 != 0
    }
}

/// TDS Packet Header
#[derive(Debug)]
pub struct PacketHeader {
    pub packet_type: PacketType,
    pub status: PacketStatus,
    pub length: u16,
    pub spid: u16,
    pub packet_id: u8,
    pub window: u8,
}

impl PacketHeader {
    pub fn new(packet_type: PacketType, length: u16, packet_id: u8) -> Self {
        Self {
            packet_type,
            status: PacketStatus::end_of_message(),
            length,
            spid: 0,
            packet_id,
            window: 0,
        }
    }

    pub fn parse(buf: &mut impl Buf) -> Result<Self, ProtocolError> {
        if buf.remaining() < PACKET_HEADER_SIZE {
            return Err(ProtocolError::Protocol(
                "Not enough data for header".to_string(),
            ));
        }

        let packet_type = PacketType::try_from(buf.get_u8())?;
        let status = PacketStatus(buf.get_u8());
        let length = buf.get_u16();
        let spid = buf.get_u16();
        let packet_id = buf.get_u8();
        let window = buf.get_u8();

        debug!(
            ?packet_type,
            ?status,
            length,
            spid,
            packet_id,
            window,
            "Parsed packet header"
        );

        Ok(Self {
            packet_type,
            status,
            length,
            spid,
            packet_id,
            window,
        })
    }

    pub fn write(&self, buf: &mut impl BufMut) {
        buf.put_u8(self.packet_type as u8);
        buf.put_u8(self.status.0);
        buf.put_u16(self.length);
        buf.put_u16(self.spid);
        buf.put_u8(self.packet_id);
        buf.put_u8(self.window);
    }
}

/// TDS Token Types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenType {
    ColMetadata = 0x81,
    Row = 0xD1,
    Done = 0xFD,
    DoneProc = 0xFE,
    DoneInProc = 0xFF,
    FedAuthInfo = 0xEE,
    EnvChange = 0xE3,
    LoginAck = 0xAD,
    Error = 0xAA,
    Info = 0xAB,
    FeatureExtAck = 0xAE,
}

/// FedAuth feature extension ID
pub const FEATURE_EXT_FEDAUTH: u8 = 0x02;
pub const FEATURE_EXT_USER_AGENT: u8 = 0x10;
/// FedAuth terminator
pub const FEATURE_EXT_TERMINATOR: u8 = 0xFF;

/// Parsed Login7 authentication info
#[derive(Debug, Clone, Default)]
pub struct Login7AuthInfo {
    /// Whether FedAuth feature was present
    pub has_fedauth: bool,
    /// Access token bytes (UTF-16LE encoded) if present
    pub access_token_bytes: Option<Vec<u8>>,
    /// FedAuth library type (0x01 = SecurityToken, 0x02 = MSAL)
    pub fedauth_library: u8,
    /// Server name from Login7 packet (the data source string sent by client)
    pub server_name: Option<String>,
    /// User Agent string from Login7 packet (if present)
    pub user_agent: Option<String>,
}

/// Parse Login7 packet to extract FedAuth feature extension with access token
pub fn parse_login7_auth(packet_data: &[u8]) -> Login7AuthInfo {
    let mut auth_info = Login7AuthInfo::default();

    // Login7 packet structure (offsets from start of Login7 body after TDS header):
    // - 4 bytes: Length (offset 0)
    // - 4 bytes: TDSVersion (offset 4)
    // - 4 bytes: PacketSize (offset 8)
    // - 4 bytes: ClientProgVer (offset 12)
    // - 4 bytes: ClientPID (offset 16)
    // - 4 bytes: ConnectionID (offset 20)
    // - 1 byte: OptionFlags1 (offset 24)
    // - 1 byte: OptionFlags2 (offset 25)
    // - 1 byte: TypeFlags (offset 26)
    // - 1 byte: OptionFlags3 (offset 27) <-- Contains FeatureExt flag
    // - 4 bytes: ClientTimezone (offset 28)
    // - 4 bytes: ClientLCID (offset 32)
    // - Variable length offset/length pairs start at offset 36
    //   Each pair is 4 bytes: 2 bytes offset (from start of packet), 2 bytes length (in chars)
    //   Order: HostName, UserName, Password, AppName, ServerName, ...
    //   ServerName is at offset 36 + 16 = 52 (5th entry, 0-indexed as 4)

    // The packet_body should already have TDS header stripped (done in server.rs)
    let data = packet_data;

    if data.len() < 56 {
        debug!("Login7 packet too short for server name parsing");
        return auth_info;
    }

    // Parse ServerName from offset table
    // ServerName offset/length is at bytes 52-55 (5th entry in offset table)
    let server_name_offset = u16::from_le_bytes([data[52], data[53]]) as usize;
    let server_name_length = u16::from_le_bytes([data[54], data[55]]) as usize; // length in chars

    if server_name_length > 0 && server_name_offset + server_name_length * 2 <= data.len() {
        // ServerName is UTF-16LE encoded, length is in characters
        let server_name_bytes =
            &data[server_name_offset..server_name_offset + server_name_length * 2];
        let u16_chars: Vec<u16> = server_name_bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        if let Ok(server_name) = String::from_utf16(&u16_chars) {
            debug!("Login7 ServerName: '{}'", server_name);
            auth_info.server_name = Some(server_name);
        }
    }

    // Read OptionFlags3 at offset 27
    let option_flags3 = data[27];
    let has_feature_ext = (option_flags3 & 0x10) != 0; // Bit 4 indicates FeatureExt presence

    debug!(
        "Login7 OptionFlags3: 0x{:02X}, has_feature_ext: {}",
        option_flags3, has_feature_ext
    );

    if !has_feature_ext {
        debug!("Login7 does not have feature extension");
        return auth_info;
    }

    // The FeatureExt entry in the Login7 offset table is at bytes 56-59
    // (6th entry: HostName@36, UserName@40, Password@44, AppName@48, ServerName@52, FeatureExt@56)
    // It contains an offset (2 bytes) and length (2 bytes) pointing to a DWORD in the variable data.
    // That DWORD is the actual offset to the feature extension list.
    if data.len() < 60 {
        debug!("Login7 packet too short for FeatureExt offset table entry");
        return auth_info;
    }

    let feat_ext_ptr_offset = u16::from_le_bytes([data[56], data[57]]) as usize;

    // Read the DWORD at feat_ext_ptr_offset — this is the actual offset to feature extension data
    if feat_ext_ptr_offset + 4 > data.len() {
        debug!(
            "FeatureExt pointer offset {} out of bounds (packet len {})",
            feat_ext_ptr_offset,
            data.len()
        );
        return auth_info;
    }

    let feature_ext_offset = u32::from_le_bytes([
        data[feat_ext_ptr_offset],
        data[feat_ext_ptr_offset + 1],
        data[feat_ext_ptr_offset + 2],
        data[feat_ext_ptr_offset + 3],
    ]) as usize;

    debug!(
        "FeatureExt pointer at offset {}, points to feature data at offset {}",
        feat_ext_ptr_offset, feature_ext_offset
    );

    if feature_ext_offset >= data.len() {
        debug!(
            "FeatureExt data offset {} out of bounds (packet len {})",
            feature_ext_offset,
            data.len()
        );
        return auth_info;
    }

    // Parse feature extension entries starting at feature_ext_offset
    // Format: 1 byte FeatureId, 4 bytes FeatureDataLen, N bytes FeatureData
    // Terminated by 0xFF
    let mut i = feature_ext_offset;
    while i < data.len() {
        let feature_id = data[i];
        if feature_id == FEATURE_EXT_TERMINATOR {
            break;
        }

        if i + 5 > data.len() {
            break;
        }

        let feat_len =
            u32::from_le_bytes([data[i + 1], data[i + 2], data[i + 3], data[i + 4]]) as usize;

        if feature_id == FEATURE_EXT_FEDAUTH {
            auth_info.has_fedauth = true;

            if i + 5 + feat_len <= data.len() {
                let feat_data = &data[i + 5..i + 5 + feat_len];

                if !feat_data.is_empty() {
                    let options = feat_data[0];
                    auth_info.fedauth_library = (options >> 1) & 0x03;

                    if auth_info.fedauth_library == 0x01 && feat_data.len() > 5 {
                        let token_len = u32::from_le_bytes([
                            feat_data[1],
                            feat_data[2],
                            feat_data[3],
                            feat_data[4],
                        ]) as usize;

                        if feat_data.len() >= 5 + token_len {
                            auth_info.access_token_bytes =
                                Some(feat_data[5..5 + token_len].to_vec());
                            debug!("Parsed access token from Login7: {} bytes", token_len);
                        }
                    }
                }
            }
        } else if feature_id == FEATURE_EXT_USER_AGENT && i + 5 + feat_len <= data.len() {
            let feat_data = &data[i + 5..i + 5 + feat_len];
            if feat_data.len().is_multiple_of(2) {
                let u16_chars: Vec<u16> = feat_data
                    .chunks_exact(2)
                    .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                    .collect();
                if let Ok(user_agent) = String::from_utf16(&u16_chars) {
                    debug!("Parsed UserAgent from Login7 (len: {})", user_agent.len());
                    auth_info.user_agent = Some(user_agent);
                }
            } else {
                debug!(
                    "Skipping UserAgent FeatureExtension due to odd data length: {}",
                    feat_len
                );
            }
        }

        // Skip to next feature entry
        i += 5 + feat_len;
    }

    auth_info
}

/// Build a PreLogin response packet
pub fn build_prelogin_response() -> BytesMut {
    build_prelogin_response_with_options(false, false)
}

/// Build a PreLogin response packet with optional encryption support
pub fn build_prelogin_response_with_encryption(supports_encryption: bool) -> BytesMut {
    build_prelogin_response_with_options(supports_encryption, false)
}

/// Build a PreLogin response packet with encryption and FedAuth support
pub fn build_prelogin_response_with_fedauth(
    supports_encryption: bool,
    supports_fedauth: bool,
) -> BytesMut {
    build_prelogin_response_with_options(supports_encryption, supports_fedauth)
}

/// Build a PreLogin response packet with configurable options
fn build_prelogin_response_with_options(
    supports_encryption: bool,
    supports_fedauth: bool,
) -> BytesMut {
    let mut response = BytesMut::new();

    // PreLogin response with ENCRYPTION and optionally FEDAUTH
    // Option tokens (PL_OPTION_TOKEN)
    //
    // Option directory format for each option:
    // - 1 byte: Option token
    // - 2 bytes: Offset (big-endian) from start of PreLogin data to option value
    // - 2 bytes: Length (big-endian) of option value

    if supports_fedauth {
        // With FEDAUTH: VERSION + ENCRYPTION + FEDAUTH + TERMINATOR
        // Directory: 5 + 5 + 5 + 1 = 16 bytes
        // VERSION data at offset 16, length 6
        // ENCRYPTION data at offset 22, length 1
        // FEDAUTH data at offset 23, length 1

        // VERSION token (0x00)
        response.put_u8(0x00); // VERSION
        response.put_u16(0x0010); // Offset 16
        response.put_u16(0x0006); // Length 6

        // ENCRYPTION token (0x01)
        response.put_u8(0x01); // ENCRYPTION
        response.put_u16(0x0016); // Offset 22
        response.put_u16(0x0001); // Length 1

        // FEDAUTH token (0x06)
        response.put_u8(0x06); // FEDAUTH
        response.put_u16(0x0017); // Offset 23
        response.put_u16(0x0001); // Length 1

        // TERMINATOR (0xFF)
        response.put_u8(0xFF);

        // Version data: 16.0.0.0 (6 bytes)
        response.put_u8(0x10); // Major version (16)
        response.put_u8(0x00); // Minor version (0)
        response.put_u16(0x0000); // Build number
        response.put_u16(0x0000); // Sub-build number

        // ENCRYPTION data (1 byte)
        if supports_encryption {
            response.put_u8(0x01); // ENCRYPT_ON
        } else {
            response.put_u8(0x02); // ENCRYPT_NOT_SUP
        }

        // FEDAUTH data (1 byte)
        // 0x00 = FEDAUTH_OFF
        // 0x01 = FEDAUTH_ON (FedAuth supported)
        response.put_u8(0x01); // FEDAUTH_ON
    } else {
        // Without FEDAUTH: VERSION + ENCRYPTION + TERMINATOR
        // Directory: 5 + 5 + 1 = 11 bytes
        // VERSION data at offset 11, length 6
        // ENCRYPTION data at offset 17, length 1

        // VERSION token (0x00)
        response.put_u8(0x00); // VERSION
        response.put_u16(0x000B); // Offset 11
        response.put_u16(0x0006); // Length 6

        // ENCRYPTION token (0x01)
        response.put_u8(0x01); // ENCRYPTION
        response.put_u16(0x0011); // Offset 17
        response.put_u16(0x0001); // Length 1

        // TERMINATOR (0xFF)
        response.put_u8(0xFF);

        // Version data: 16.0.0.0 (6 bytes)
        response.put_u8(0x10); // Major version (16)
        response.put_u8(0x00); // Minor version (0)
        response.put_u16(0x0000); // Build number
        response.put_u16(0x0000); // Sub-build number

        // ENCRYPTION data (1 byte)
        if supports_encryption {
            response.put_u8(0x01); // ENCRYPT_ON
        } else {
            response.put_u8(0x02); // ENCRYPT_NOT_SUP
        }
    }

    wrap_in_packet(PacketType::TabularResult, response)
}

/// Build a FeatureExtAck token for FedAuth acknowledgment
pub fn build_feature_ext_ack_fedauth() -> BytesMut {
    let mut token_data = BytesMut::new();

    // FeatureExtAck token (0xAE)
    token_data.put_u8(TokenType::FeatureExtAck as u8);

    // FedAuth feature (0x02) with empty data (just acknowledge)
    token_data.put_u8(FEATURE_EXT_FEDAUTH); // Feature ID
    token_data.put_u32_le(0); // Feature data length = 0

    // Terminator
    token_data.put_u8(FEATURE_EXT_TERMINATOR);

    token_data
}

/// Parse a FedAuthToken packet payload and extract the raw token bytes.
pub fn parse_fedauth_token(packet_data: &[u8]) -> Result<Vec<u8>, ProtocolError> {
    if packet_data.len() < 8 {
        return Err(ProtocolError::Protocol(
            "FedAuthToken packet too short".to_string(),
        ));
    }

    let total_length =
        u32::from_le_bytes(packet_data[0..4].try_into().expect("slice with known size")) as usize;
    let token_length =
        u32::from_le_bytes(packet_data[4..8].try_into().expect("slice with known size")) as usize;

    if total_length + size_of::<u32>() != packet_data.len() {
        return Err(ProtocolError::Protocol(format!(
            "FedAuthToken payload length mismatch: declared {total_length}, actual {}",
            packet_data.len()
        )));
    }

    if total_length != token_length + size_of::<u32>() {
        return Err(ProtocolError::Protocol(format!(
            "FedAuthToken inner length mismatch: declared {token_length} token bytes inside {total_length} byte payload"
        )));
    }

    Ok(packet_data[8..].to_vec())
}

/// Build a TabularResult packet containing a FedAuth challenge.
pub fn build_fedauth_challenge_response(sts_url: &str, spn: &str) -> BytesMut {
    let mut response = build_feature_ext_ack_fedauth();
    response.extend_from_slice(&build_fedauth_info_token(sts_url, spn));
    wrap_in_packet(PacketType::TabularResult, response)
}

fn build_fedauth_info_token(sts_url: &str, spn: &str) -> BytesMut {
    let options = [(0x01_u8, sts_url), (0x02_u8, spn)];
    let mut token_data = BytesMut::new();
    token_data.put_u8(TokenType::FedAuthInfo as u8);

    let option_headers_len = options.len() * 9;
    let encoded_values: Vec<Vec<u8>> = options
        .iter()
        .map(|(_, value)| {
            value
                .encode_utf16()
                .flat_map(|unit| unit.to_le_bytes())
                .collect::<Vec<u8>>()
        })
        .collect();
    let string_bytes_len: usize = encoded_values.iter().map(Vec::len).sum();
    let payload_len = size_of::<u32>() + option_headers_len + string_bytes_len;

    token_data.put_i32_le(payload_len as i32);
    token_data.put_u32_le(options.len() as u32);

    let mut current_offset = (size_of::<u32>() + option_headers_len) as u32;
    for ((option_id, _), encoded) in options.iter().zip(encoded_values.iter()) {
        token_data.put_u8(*option_id);
        token_data.put_u32_le(encoded.len() as u32);
        token_data.put_u32_le(current_offset);
        current_offset += encoded.len() as u32;
    }

    for encoded in encoded_values {
        token_data.extend_from_slice(&encoded);
    }

    token_data
}

/// Build a LoginAck token response with EnvChange for collation
pub fn build_login_ack() -> BytesMut {
    let mut token_data = BytesMut::new();

    // LoginAck token (0xAD)
    token_data.put_u8(TokenType::LoginAck as u8);

    // Calculate token length (we'll update this) - USHORT in little-endian
    let length_pos = token_data.len();
    token_data.put_u16_le(0); // Placeholder for length

    // Interface (SQL Server)
    token_data.put_u8(0x01);

    // TDS version (7.4) - sent as big-endian (network byte order)
    token_data.put_u32(0x74000004);

    // Program name: "MockTdsServer"
    let prog_name = "MockTdsServer";
    token_data.put_u8(prog_name.len() as u8);
    for ch in prog_name.encode_utf16() {
        token_data.put_u16_le(ch);
    }

    // Server version (16.0.0.0)
    token_data.put_u8(16); // Major
    token_data.put_u8(0); // Minor
    token_data.put_u16_le(0); // Build (little-endian)

    // Update the length field (little-endian)
    let token_length = (token_data.len() - length_pos - 2) as u16;
    token_data[length_pos] = (token_length & 0xFF) as u8;
    token_data[length_pos + 1] = ((token_length >> 8) & 0xFF) as u8;

    // Add EnvChange token for collation (0xE3)
    token_data.put_u8(TokenType::EnvChange as u8);

    // EnvChange token length (little-endian)
    let env_length_pos = token_data.len();
    token_data.put_u16_le(0); // Placeholder

    // Type: SQL_COLLATION (7)
    token_data.put_u8(7);

    // New value length: 5 bytes for collation
    token_data.put_u8(5);

    // Collation data: SQL_Latin1_General_CP1_CI_AS (LCID: 0x0409, flags: 0x00D001, sortId: 0x00)
    token_data.put_u32_le(0x09040000); // LCID (little endian)
    token_data.put_u8(0xD0); // Flags

    // Old value length: 0 (no old value)
    token_data.put_u8(0);

    // Update EnvChange length (little-endian)
    let env_token_length = (token_data.len() - env_length_pos - 2) as u16;
    token_data[env_length_pos] = (env_token_length & 0xFF) as u8;
    token_data[env_length_pos + 1] = ((env_token_length >> 8) & 0xFF) as u8;

    // Add EnvChange token for database name (0xE3)
    token_data.put_u8(TokenType::EnvChange as u8);

    // EnvChange token length (little-endian)
    let db_env_length_pos = token_data.len();
    token_data.put_u16_le(0); // Placeholder

    // Type: DATABASE (1)
    token_data.put_u8(1);

    // New database name: "master"
    let db_name = "master";
    token_data.put_u8(db_name.len() as u8);
    for ch in db_name.encode_utf16() {
        token_data.put_u16_le(ch);
    }

    // Old value length: 0 (no old value)
    token_data.put_u8(0);

    // Update EnvChange length (little-endian)
    let db_env_token_length = (token_data.len() - db_env_length_pos - 2) as u16;
    token_data[db_env_length_pos] = (db_env_token_length & 0xFF) as u8;
    token_data[db_env_length_pos + 1] = ((db_env_token_length >> 8) & 0xFF) as u8;

    // Add EnvChange token for packet size (0xE3)
    token_data.put_u8(TokenType::EnvChange as u8);

    // EnvChange token length (little-endian)
    let ps_env_length_pos = token_data.len();
    token_data.put_u16_le(0); // Placeholder

    // Type: PACKETSIZE (4)
    token_data.put_u8(4);

    // New packet size: "4096" (as string in UTF-16LE)
    let new_packet_size_str = "4096";
    token_data.put_u8(new_packet_size_str.len() as u8);
    for ch in new_packet_size_str.encode_utf16() {
        token_data.put_u16_le(ch);
    }

    // Old packet size: "4096" (as string in UTF-16LE)
    let old_packet_size_str = "4096";
    token_data.put_u8(old_packet_size_str.len() as u8);
    for ch in old_packet_size_str.encode_utf16() {
        token_data.put_u16_le(ch);
    }

    // Update EnvChange length (little-endian)
    let ps_env_token_length = (token_data.len() - ps_env_length_pos - 2) as u16;
    token_data[ps_env_length_pos] = (ps_env_token_length & 0xFF) as u8;
    token_data[ps_env_length_pos + 1] = ((ps_env_token_length >> 8) & 0xFF) as u8;

    token_data
}

/// Build a DONE token
pub fn build_done_token(row_count: u64) -> BytesMut {
    let mut token_data = BytesMut::new();

    // DONE token (0xFD)
    token_data.put_u8(TokenType::Done as u8);

    // Status: DONE_FINAL (0x00) - little-endian
    token_data.put_u16_le(0x0000);

    // CurCmd: SELECT (0xC1) - little-endian
    token_data.put_u16_le(0x00C1);

    // RowCount (8 bytes)
    token_data.put_u64_le(row_count);

    token_data
}

/// Build an INFO token (0xAB) matching the TDS wire format.
///
/// Wire layout (all integers little-endian):
///   0xAB | u16 length | u32 number | u8 state | u8 severity
///   | US_VARCHAR message | B_VARCHAR server_name | B_VARCHAR proc_name | u32 line_number
///
/// US_VARCHAR: u16 char-count prefix + UTF-16LE data
/// B_VARCHAR:  u8  char-count prefix + UTF-16LE data
pub fn build_info_token(info: &crate::query_response::InfoMessage) -> BytesMut {
    let mut buf = BytesMut::new();

    buf.put_u8(TokenType::Info as u8);

    let msg_units: Vec<u16> = info.message.encode_utf16().collect();
    let srv_units: Vec<u16> = info.server_name.encode_utf16().collect();
    let proc_units: Vec<u16> = info.proc_name.encode_utf16().collect();

    // token_len covers everything after the 2-byte length field
    let token_len: u16 = 4 + 1 + 1 // number, state, severity
        + 2 + (msg_units.len() * 2) as u16  // US_VARCHAR
        + 1 + (srv_units.len() * 2) as u16  // B_VARCHAR
        + 1 + (proc_units.len() * 2) as u16 // B_VARCHAR
        + 4; // line_number
    buf.put_u16_le(token_len);

    buf.put_u32_le(info.number);
    buf.put_u8(info.state);
    buf.put_u8(info.severity);

    buf.put_u16_le(msg_units.len() as u16);
    for u in &msg_units {
        buf.put_u16_le(*u);
    }

    buf.put_u8(srv_units.len() as u8);
    for u in &srv_units {
        buf.put_u16_le(*u);
    }

    buf.put_u8(proc_units.len() as u8);
    for u in &proc_units {
        buf.put_u16_le(*u);
    }

    buf.put_u32_le(info.line_number);
    buf
}

/// Build a query result from a QueryResponse
pub fn build_query_result(response: &crate::query_response::QueryResponse) -> BytesMut {
    let mut result = BytesMut::new();

    // ColMetadata token (0x81)
    result.put_u8(TokenType::ColMetadata as u8);
    result.put_u16_le(response.columns.len() as u16); // Column count

    // Serialize each column
    for col in &response.columns {
        result.put_u32_le(0); // UserType
        result.put_u16_le(0x0000); // Flags: not nullable, no special flags
        result.put_u8(col.data_type.tds_type_code());
        if col.data_type == crate::query_response::SqlDataType::NVarChar {
            // Required to support string responses (e.g., @@USERAGENT).
            // TDS ColMetadata mandates a 5-byte collation suffix for variable-length types.
            result.put_u16_le(8000); // NVARCHAR(4000) max byte capacity
            result.put_slice(&[0x09, 0x04, 0xD0, 0x00, 0x34]); // SQL_Latin1_General_CP1_CI_AS
        } else {
            result.put_u8(col.data_type.max_length());
        }

        // Column name (UTF-16LE)
        let name_len = col.name.chars().count() as u8;
        result.put_u8(name_len);
        for ch in col.name.encode_utf16() {
            result.put_u16_le(ch);
        }
    }

    // Inject Info tokens between ColMetadata and Rows (Fabric DW behavior)
    for info in &response.info_tokens {
        result.extend_from_slice(&build_info_token(info));
    }

    // Serialize each row
    for row in &response.rows {
        result.put_u8(TokenType::Row as u8);
        for value in &row.values {
            value.write_to_buffer(&mut result);
        }
    }

    // DONE token
    result.extend_from_slice(&build_done_token(response.rows.len() as u64));

    wrap_in_packet(PacketType::TabularResult, result)
}

/// Build an error response
pub fn build_error_response(message: &str) -> BytesMut {
    let mut response = BytesMut::new();

    // Error token (0xAA)
    response.put_u8(TokenType::Error as u8);

    // Calculate token length (we'll update this)
    let length_pos = response.len();
    response.put_u16(0); // Placeholder for length

    // Error number
    response.put_u32_le(50000);

    // State
    response.put_u8(1);

    // Class (severity)
    response.put_u8(16);

    // Message
    response.put_u16_le(message.len() as u16);
    for ch in message.encode_utf16() {
        response.put_u16_le(ch);
    }

    // Server name (empty)
    response.put_u8(0);

    // Procedure name (empty)
    response.put_u8(0);

    // Line number
    response.put_u32_le(1);

    // Update the length field
    let token_length = (response.len() - length_pos - 2) as u16;
    let mut length_bytes = &mut response[length_pos..length_pos + 2];
    length_bytes.put_u16(token_length);

    // Add DONE token
    response.extend_from_slice(&build_done_token(0));

    wrap_in_packet(PacketType::TabularResult, response)
}

/// Wrap token data in a TDS packet
fn wrap_in_packet(packet_type: PacketType, data: BytesMut) -> BytesMut {
    let total_length = (PACKET_HEADER_SIZE + data.len()) as u16;

    let mut packet = BytesMut::with_capacity(total_length as usize);
    let header = PacketHeader::new(packet_type, total_length, 1);
    header.write(&mut packet);
    packet.extend_from_slice(&data);

    trace!(
        "Built packet: type={:?}, length={}",
        packet_type, total_length
    );
    packet
}

/// EnvChange subtype for Routing
pub const ENVCHANGE_ROUTING: u8 = 20; // 0x14

/// Build a routing EnvChange token for connection redirection
///
/// This token is sent by the server during login to redirect the client
/// to a different server endpoint (e.g., in Azure SQL Database scenarios).
///
/// # Arguments
/// * `redirect_host` - The hostname of the server to redirect to
/// * `redirect_port` - The port of the server to redirect to
///
/// # Returns
/// A BytesMut containing the complete EnvChange routing token
pub fn build_routing_envchange_token(redirect_host: &str, redirect_port: u16) -> BytesMut {
    let mut token_data = BytesMut::new();

    // EnvChange token (0xE3)
    token_data.put_u8(TokenType::EnvChange as u8);

    // Calculate lengths
    // Server name in UTF-16LE: length prefix (2 bytes) + chars (2 bytes each)
    let server_utf16: Vec<u16> = redirect_host.encode_utf16().collect();
    let server_bytes_len = 2 + (server_utf16.len() * 2); // u16 length + UTF-16LE chars

    // New routing value: protocol (1) + port (2) + server name with length
    let new_value_len = 1 + 2 + server_bytes_len;

    // Token body: subtype (1) + new_value_length (2) + new_value + old_value_length (2)
    let token_body_len = 1 + 2 + new_value_len + 2;

    // Token length (little-endian)
    token_data.put_u16_le(token_body_len as u16);

    // Subtype: Routing (0x14 = 20)
    token_data.put_u8(ENVCHANGE_ROUTING);

    // New value length (little-endian)
    token_data.put_u16_le(new_value_len as u16);

    // Protocol: TCP (0x00)
    token_data.put_u8(0x00);

    // Port (little-endian)
    token_data.put_u16_le(redirect_port);

    // Server name: u16 length in chars, then UTF-16LE encoded string
    token_data.put_u16_le(server_utf16.len() as u16);
    for ch in server_utf16 {
        token_data.put_u16_le(ch);
    }

    // Old value length: 0 (no old routing info)
    token_data.put_u16_le(0);

    debug!(
        "Built routing EnvChange token: redirect to {}:{}",
        redirect_host, redirect_port
    );

    token_data
}

/// Build a redirection response for login
///
/// This response is sent instead of a normal login response when the server
/// wants to redirect the client to a different endpoint. The response contains:
/// - Routing EnvChange token (tells client where to connect)
/// - DONE token (indicates end of response)
///
/// # Arguments
/// * `redirect_host` - The hostname to redirect to
/// * `redirect_port` - The port to redirect to
///
/// # Returns
/// A BytesMut containing the complete TDS packet with routing response
pub fn build_routing_response(redirect_host: &str, redirect_port: u16) -> BytesMut {
    let mut response = BytesMut::new();

    // Add routing EnvChange token
    response.extend_from_slice(&build_routing_envchange_token(redirect_host, redirect_port));

    // Add DONE token
    response.extend_from_slice(&build_done_token(0));

    wrap_in_packet(PacketType::TabularResult, response)
}

/// Parse a SQL batch from packet data
pub fn parse_sql_batch(data: &[u8]) -> Result<String, ProtocolError> {
    if data.is_empty() {
        return Err(ProtocolError::Protocol("Empty SQL batch".to_string()));
    }

    // Skip the ALL_HEADERS section
    let mut offset = 0;

    // Read total length of ALL_HEADERS (DWORD)
    if data.len() < 4 {
        return Err(ProtocolError::Protocol(
            "Invalid SQL batch format".to_string(),
        ));
    }

    let all_headers_len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    offset += all_headers_len;

    if offset > data.len() {
        return Err(ProtocolError::Protocol(
            "Invalid ALL_HEADERS length".to_string(),
        ));
    }

    // The rest is the SQL text in UTF-16LE
    let sql_bytes = &data[offset..];

    // Convert UTF-16LE to String
    let u16_vec: Vec<u16> = sql_bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    let sql = String::from_utf16(&u16_vec)
        .map_err(|_| ProtocolError::Protocol("Invalid UTF-16 in SQL batch".to_string()))?;

    debug!("Parsed SQL: {}", sql.trim());
    Ok(sql.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packet_header_parse() {
        let mut buf = BytesMut::new();
        buf.put_u8(0x01); // SqlBatch
        buf.put_u8(0x01); // EOM
        buf.put_u16(100); // Length
        buf.put_u16(0); // SPID
        buf.put_u8(1); // Packet ID
        buf.put_u8(0); // Window

        let header = PacketHeader::parse(&mut buf)
            .expect("packet header parsing should succeed for a valid test packet");
        assert_eq!(header.packet_type, PacketType::SqlBatch);
        assert!(header.status.is_end_of_message());
        assert_eq!(header.length, 100);
    }

    #[test]
    fn test_prelogin_response() {
        let response = build_prelogin_response();
        assert!(response.len() >= PACKET_HEADER_SIZE);
    }

    #[test]
    fn test_select_one_result() {
        use crate::query_response::QueryResponse;
        let response = build_query_result(&QueryResponse::select_one());
        assert!(response.len() >= PACKET_HEADER_SIZE);
    }

    #[test]
    fn test_routing_envchange_token() {
        let token = build_routing_envchange_token("sqlserver.database.windows.net", 1433);

        // Token should start with EnvChange token type (0xE3)
        assert_eq!(token[0], TokenType::EnvChange as u8);

        // Followed by length (u16 LE), then subtype (0x14 = 20)
        assert_eq!(token[3], ENVCHANGE_ROUTING);

        // Should have reasonable length
        assert!(token.len() > 10);
    }

    #[test]
    fn test_routing_response() {
        let response = build_routing_response("sqlserver.database.windows.net", 1433);

        // Should be a complete TDS packet
        assert!(response.len() >= PACKET_HEADER_SIZE);

        // First byte should be packet type (TabularResult = 0x04)
        assert_eq!(response[0], PacketType::TabularResult as u8);

        // After header, should have EnvChange token (0xE3)
        assert_eq!(response[PACKET_HEADER_SIZE], TokenType::EnvChange as u8);
    }

    #[test]
    fn test_parse_login7_user_agent() {
        let user_agent = "TestAgent";
        let utf16_bytes: Vec<u8> = user_agent
            .encode_utf16()
            .flat_map(|ch| ch.to_le_bytes().into_iter())
            .collect();

        let mut payload = BytesMut::zeroed(100);

        // OptionFlags3 at byte 27: set FeatureExt bit (0x10)
        payload[27] = 0x10;

        // FeatureExt table entry at bytes 56-57 (points to offset 60)
        payload[56] = 60;
        payload[57] = 0;

        // DWORD at byte 60 pointing to the actual feature data at byte 64
        payload[60..64].copy_from_slice(&64u32.to_le_bytes());

        // Truncate the buffer at 64 so we can append the feature bytes directly
        payload.truncate(64);

        // Append UserAgent feature
        payload.put_u8(FEATURE_EXT_USER_AGENT); // 0x10
        payload.put_u32_le(utf16_bytes.len() as u32);
        payload.put_slice(&utf16_bytes);
        payload.put_u8(FEATURE_EXT_TERMINATOR); // 0xFF

        let result = parse_login7_auth(&payload);
        assert_eq!(
            result
                .user_agent
                .expect("user agent should be parsed from the feature extension"),
            "TestAgent"
        );
    }

    #[test]
    fn test_parse_login7_odd_length_user_agent() {
        let mut payload = BytesMut::zeroed(100);
        payload[27] = 0x10; // OptionFlags3 (FeatureExt bit)
        payload[56] = 60; // Option offset
        payload[57] = 0;
        payload[60..64].copy_from_slice(&64u32.to_le_bytes()); // Feature offset
        payload.truncate(64);

        // FeatureId (1 byte) = 2 (UserAgent)
        // FeatureDataLen (4 bytes) = 5 (odd length)
        // FeatureData (5 bytes) = [1, 2, 3, 4, 5]
        payload.put_u8(FEATURE_EXT_USER_AGENT); // 2
        payload.put_u32_le(5);
        payload.put_slice(&[1, 2, 3, 4, 5]);
        payload.put_u8(FEATURE_EXT_TERMINATOR); // 0xFF

        let result = parse_login7_auth(&payload);
        assert_eq!(result.user_agent, None);
    }
}
