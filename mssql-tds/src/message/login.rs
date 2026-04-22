// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::connection::client_context::{
    ClientContext, TdsAuthenticationMethod, TransportContext, VectorVersion,
};
use crate::message::features::jsonfeature::JsonFeature;
use crate::message::login_options::{
    OptionFlags1, OptionFlags2, OptionFlags3, OptionsValue, TdsVersion, TypeFlags,
};
use crate::message::messages::{PacketType, Request, TdsError};

use crate::io::packet_writer::{PacketWriter, TdsPacketWriter};
use crate::token::fed_auth_info::{FedAuthInfoToken, SspiToken};
use crate::token::login_ack::LoginAckToken;
use crate::token::tokens::{
    EnvChangeContainer, EnvChangeToken, EnvChangeTokenSubType, SqlCollation, Token, Tokens,
};
use async_trait::async_trait;
use std::collections::HashMap;
use std::fmt::Debug;

use super::features::fedauth::FedAuthFeature;
use super::features::useragent::UserAgentFeature;
use super::features::utf8::Utf8Feature;
use super::features::vectorfeature::VectorFeature;
use crate::core::TdsResult;
use crate::io::token_stream::{ParserContext, TdsTokenStreamReader};
use tracing::{Level, debug, event, info, trace};

pub(crate) const FIXED_LOGIN_RECORD_LENGTH: i32 = 94;

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoutingInfo {
    pub protocol: u8,
    pub port: u16,
    pub server: String,
}

#[derive(Default, Debug, Clone)]
pub(crate) struct EnvChangeProperties {
    pub database_collation: Option<SqlCollation>,
    pub packet_size: i32,
    pub language: Option<String>,
    pub database: Option<String>,
    pub char_set: Option<String>,
    pub routing_information: Option<RoutingInfo>,
}

#[derive(Eq, PartialEq, Hash, Debug, Copy, Clone)]
pub(crate) enum FeatureExtension {
    SRecovery,
    FedAuth,
    AlwaysEncrypted,
    GlobalTransactions,
    AzureSqlSupport,
    DataClassification,
    Utf8Support,
    SqlDnsCaching,
    Json,
    Vector,
    UserAgent,
    Terminator,
    Unknown(u8),
}

impl FeatureExtension {
    pub fn as_u8(self) -> u8 {
        match self {
            FeatureExtension::SRecovery => 0x01,
            FeatureExtension::FedAuth => 0x02,
            FeatureExtension::AlwaysEncrypted => 0x04,
            FeatureExtension::GlobalTransactions => 0x05,
            FeatureExtension::AzureSqlSupport => 0x08,
            FeatureExtension::DataClassification => 0x09,
            FeatureExtension::Utf8Support => 0x0A,
            FeatureExtension::SqlDnsCaching => 0x0B,
            FeatureExtension::Json => 0x0D,
            FeatureExtension::Vector => 0x0E,
            FeatureExtension::UserAgent => 0x10,
            FeatureExtension::Terminator => 0xFF,
            FeatureExtension::Unknown(value) => value,
        }
    }
}

impl From<u8> for FeatureExtension {
    fn from(value: u8) -> Self {
        match value {
            0x01 => FeatureExtension::SRecovery,
            0x02 => FeatureExtension::FedAuth,
            0x04 => FeatureExtension::AlwaysEncrypted,
            0x05 => FeatureExtension::GlobalTransactions,
            0x08 => FeatureExtension::AzureSqlSupport,
            0x09 => FeatureExtension::DataClassification,
            0x0A => FeatureExtension::Utf8Support,
            0x0B => FeatureExtension::SqlDnsCaching,
            0x0D => FeatureExtension::Json,
            0x0E => FeatureExtension::Vector,
            0x10 => FeatureExtension::UserAgent,
            0xFF => FeatureExtension::Terminator,
            _ => FeatureExtension::Unknown(value),
        }
    }
}

#[async_trait]
pub(crate) trait Feature: Send + Sync + Debug {
    fn feature_identifier(&self) -> FeatureExtension;
    fn is_requested(&self) -> bool;
    fn data_length(&self) -> i32;
    async fn serialize(&self, packet_writer: &mut PacketWriter) -> TdsResult<()>;
    fn deserialize(&mut self, data: &[u8]) -> TdsResult<()>;

    #[allow(dead_code)]
    // This method is not used currently, and exists for completeness.
    fn is_acknowledged(&self) -> bool;
    fn set_acknowledged(&mut self, _acknowledged: bool);
    fn clone_box(&self) -> Box<dyn Feature>;
}

// Implement Clone for Box<dyn Feature>
impl Clone for Box<dyn Feature> {
    fn clone(&self) -> Box<dyn Feature> {
        self.clone_box()
    }
}

// Implement Clone for FeaturesRequest
impl Clone for FeaturesRequest {
    fn clone(&self) -> Self {
        FeaturesRequest {
            features: self.features.iter().map(|(k, v)| (*k, v.clone())).collect(),
        }
    }
}

pub(crate) struct FeaturesRequest {
    pub features: HashMap<FeatureExtension, Box<dyn Feature>>,
}

impl FeaturesRequest {
    pub fn build(
        authentication_options: TdsAuthenticationMethod,
        access_token: Option<String>,
        prelogin_fedauth_response: bool,
        vector_version: VectorVersion,
        user_agent_feature: UserAgentFeature,
    ) -> Self {
        let mut features: HashMap<FeatureExtension, Box<dyn Feature>> = HashMap::new();
        features.insert(
            FeatureExtension::Utf8Support,
            Box::new(Utf8Feature::default()),
        );

        features.insert(FeatureExtension::Json, Box::new(JsonFeature::default()));

        features.insert(FeatureExtension::UserAgent, Box::new(user_agent_feature));
        if let Some(vector_feature) = Option::<VectorFeature>::from(vector_version) {
            features.insert(FeatureExtension::Vector, Box::new(vector_feature));
        }

        if authentication_options != TdsAuthenticationMethod::SSPI
            && authentication_options != TdsAuthenticationMethod::Password
        {
            features.insert(
                FeatureExtension::FedAuth,
                Box::new(FedAuthFeature::new(
                    authentication_options,
                    access_token,
                    prelogin_fedauth_response,
                )),
            );
        }
        FeaturesRequest { features }
    }

    #[allow(dead_code)]
    // This method is not used currently, and exists for completeness.
    pub fn features(&self) -> Vec<&dyn Feature> {
        self.features
            .values()
            .map(|f| f.as_ref())
            .collect::<Vec<&dyn Feature>>()
    }

    #[allow(dead_code)]
    // This method is not used currently, and exists for completeness.
    pub fn is_acknowledged(&self, _feature_extension: FeatureExtension) -> Option<&dyn Feature> {
        let feature = self.features.get(&_feature_extension);
        match feature {
            Some(f) => {
                if f.is_acknowledged() {
                    Some(f.as_ref())
                } else {
                    None
                }
            }
            None => {
                event!(
                    Level::WARN,
                    "Feature {:?} not found in the features request",
                    _feature_extension
                );
                None
            }
        }
    }

    pub fn set_acknowledged(
        &mut self,
        _feature_extension: FeatureExtension,
        _data: &[u8],
    ) -> TdsResult<()> {
        let feature = self.features.get_mut(&_feature_extension);
        match feature {
            Some(f) => {
                f.set_acknowledged(true);
                f.deserialize(_data)?;
                Ok(())
            }
            None => {
                event!(
                    Level::WARN,
                    "Feature {:?} not found in the features request when setting acknowledged",
                    _feature_extension
                );
                Ok(())
            }
        }
    }

    pub fn get_requested_features(&self) -> Vec<&dyn Feature> {
        self.features
            .values()
            .filter(|f| f.is_requested())
            .map(|f| f.as_ref())
            .collect()
    }

    #[allow(dead_code)]
    // This method is not used currently, and exists for completeness.
    pub fn get_acknowledged_features(&self) -> Vec<&dyn Feature> {
        self.features
            .values()
            .filter(|f| f.is_acknowledged())
            .map(|f| f.as_ref())
            .collect()
    }
}

impl From<(&ClientContext, bool)> for FeaturesRequest {
    fn from(context_and_prelogin_fedauth_flag: (&ClientContext, bool)) -> Self {
        let context = context_and_prelogin_fedauth_flag.0;
        FeaturesRequest::build(
            context.tds_authentication_method.clone(),
            context.access_token.clone(),
            context_and_prelogin_fedauth_flag.1,
            context.vector_version,
            UserAgentFeature::new(context),
        )
    }
}

#[derive(Default)]
pub struct PhysicalAddress {
    address_bytes: [u8; 6],
}

pub(crate) struct LoginRequestModel<'context> {
    pub option_flags1: OptionFlags1,
    pub option_flags2: OptionFlags2,
    pub option_flags3: OptionFlags3,
    pub type_flags: TypeFlags,
    pub tds_version: TdsVersion,
    // TODO: user_input needs to be login specific user_input. Need to understand how to replicate the C# concept here.
    pub user_input: &'context ClientContext,
    // This is a transport context, which will be used to get the authoritative server name.
    pub transport_context: &'context TransportContext,
    pub features_request: FeaturesRequest,
    pub client_prog_ver: i32,
    pub client_process_id: i32,
    pub connection_id_deprecated: i32,
    pub client_time_zone_deprecated: i32,
    pub client_lcid_deprecated: i32,
    pub client_id: PhysicalAddress,
    /// SSPI token data for integrated authentication.
    /// Contains the initial security token (e.g., NTLM Type 1 or Kerberos AP_REQ).
    /// None for password authentication.
    pub sspi_token: Option<Vec<u8>>,
}

impl LoginRequestModel<'_> {
    pub(crate) fn from_context<'a, 'b>(
        context: &'a ClientContext,
        pre_login_fedauth_response: bool,
        transport_context: &'b TransportContext,
    ) -> LoginRequestModel<'b>
    where
        'a: 'b,
    {
        LoginRequestModel {
            option_flags1: OptionFlags1::default(),
            option_flags2: context.into(),
            option_flags3: context.into(),
            type_flags: context.into(),
            tds_version: context.tds_version(),
            features_request: (context, pre_login_fedauth_response).into(),
            user_input: context,
            transport_context,
            client_prog_ver: context.encode_driver_version(),
            client_process_id: 0,
            connection_id_deprecated: 0,
            client_time_zone_deprecated: 0,
            client_lcid_deprecated: 0,
            client_id: PhysicalAddress::default(),
            sspi_token: None, // Set by caller for SSPI authentication
        }
    }

    /// Creates a LoginRequestModel with an SSPI token for integrated authentication.
    pub(crate) fn from_context_with_sspi<'a, 'b>(
        context: &'a ClientContext,
        pre_login_fedauth_response: bool,
        transport_context: &'b TransportContext,
        sspi_token: Vec<u8>,
    ) -> LoginRequestModel<'b>
    where
        'a: 'b,
    {
        let mut model = Self::from_context(context, pre_login_fedauth_response, transport_context);
        model.sspi_token = Some(sspi_token);
        model
    }
}

pub(crate) struct LoginResponseModel {
    pub change_properties: EnvChangeProperties,
    pub features: FeaturesRequest,
    pub tds_error: Option<TdsError>,
    pub success_token: Option<LoginAckToken>,
    pub fed_auth_info: Option<FedAuthInfoToken>,
    /// SSPI challenge token from server for integrated authentication
    pub sspi_token: Option<SspiToken>,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum LoginResponseStatus {
    #[allow(dead_code)]
    // Not used, but kept for completeness.
    NoResponse = 0x00,
    Success = 0x01,
    Error = 0x02,
    WaitingForFedAuth = 0x03,
    Rerouting = 0x04,
    /// Server sent SSPI challenge token, waiting for client to respond
    WaitingForSspi = 0x05,
}

impl LoginResponseModel {
    fn new(features: FeaturesRequest) -> Self {
        LoginResponseModel {
            change_properties: EnvChangeProperties::default(),
            features,
            tds_error: None,
            success_token: None,
            fed_auth_info: None,
            sspi_token: None,
        }
    }

    fn capture_change_property(&mut self, change_token: EnvChangeToken) -> TdsResult<()> {
        let sub_type = change_token.sub_type;
        event!(
            Level::DEBUG,
            "Capturing change property: {:?} with sub type: {:?}",
            change_token.change_type,
            sub_type
        );
        match change_token.change_type {
            EnvChangeContainer::String(string_change) => match sub_type {
                EnvChangeTokenSubType::Database => {
                    self.change_properties.database =
                        Option::from(string_change.new_value().clone());
                    Ok(())
                }
                EnvChangeTokenSubType::Language => {
                    self.change_properties.language =
                        Option::from(string_change.new_value().clone());
                    Ok(())
                }
                EnvChangeTokenSubType::CharacterSet => {
                    self.change_properties.char_set = Some(string_change.new_value().clone());
                    Ok(())
                }
                _ => {
                    event!(
                        Level::ERROR,
                        "Env change token of type string, but implementation doesn't account for this property type: {:?}",
                        sub_type
                    );
                    Err(crate::error::Error::ProtocolError(
                        "Env change token of type string, but implementation doesn't account for this property type".to_string(),
                    ))
                }
            },
            EnvChangeContainer::SqlCollation(collation_change) => {
                self.change_properties.database_collation = *collation_change.new_value();
                Ok(())
            }
            EnvChangeContainer::UInt32(numeric_change) => match sub_type {
                EnvChangeTokenSubType::PacketSize => {
                    self.change_properties.packet_size = *numeric_change.new_value() as i32;
                    Ok(())
                }
                _ => {
                    event!(
                        Level::DEBUG,
                        "Unaccounted numeric change property type: {:?}",
                        sub_type
                    );
                    Err(crate::error::Error::ProtocolError(format!(
                        "Unaccounted numeric change property type: {sub_type:?}"
                    )))
                }
            },
            EnvChangeContainer::RoutingType(routing_change) => {
                self.change_properties.routing_information = routing_change.new_value().clone();
                Err(crate::error::Error::Redirection {
                    host: routing_change.new_value().as_ref().unwrap().server.clone(),
                    port: routing_change.new_value().as_ref().unwrap().port,
                })
            }
            _ => {
                event!(
                    Level::ERROR,
                    "Unknown change property type: {:?}",
                    change_token.change_type
                );
                Err(crate::error::Error::ProtocolError(
                    "Unknown change property type".to_string(),
                ))
            }
        }
    }

    pub(crate) fn get_status(&self) -> LoginResponseStatus {
        if self.change_properties.routing_information.is_some() {
            return LoginResponseStatus::Rerouting;
        }

        if self.success_token.is_some() {
            return LoginResponseStatus::Success;
        }

        if self.tds_error.is_some() {
            return LoginResponseStatus::Error;
        }

        if self.fed_auth_info.is_some() {
            return LoginResponseStatus::WaitingForFedAuth;
        }

        if self.sspi_token.is_some() {
            return LoginResponseStatus::WaitingForSspi;
        }

        LoginResponseStatus::Error
    }
}

pub(crate) struct LoginRequest<'context> {
    pub model: LoginRequestModel<'context>,
}

#[async_trait]
impl Request for LoginRequest<'_> {
    fn packet_type(&self) -> PacketType {
        PacketType::Login7
    }

    async fn serialize<'a, 'b>(&'a self, writer: &'a mut PacketWriter<'b>) -> TdsResult<()>
    where
        'b: 'a,
    {
        Serializer::new(&self.model, writer).serialize().await
    }
}

pub(crate) struct FedAuthTokenRequest {
    pub access_token_bytes: Vec<u8>,
}

#[async_trait]
impl Request for FedAuthTokenRequest {
    fn packet_type(&self) -> PacketType {
        PacketType::FedAuthToken
    }

    async fn serialize<'a, 'b>(&'a self, writer: &'a mut PacketWriter<'b>) -> TdsResult<()>
    where
        'b: 'a,
    {
        // The token bytes as well as the length of the token bytes are written to the packet.
        let len = self.access_token_bytes.len() + size_of::<u32>();
        writer.write_u32_async(len as u32).await?;
        writer
            .write_u32_async(self.access_token_bytes.len() as u32)
            .await?;
        writer.write_async(&self.access_token_bytes).await?;
        writer.finalize().await?;
        Ok(())
    }
}

/// SSPI continuation message for multi-round authentication.
///
/// This is sent as packet type 0x11 (SSPI) when responding to
/// server SSPI challenges during integrated authentication.
pub(crate) struct SspiRequest {
    /// The SSPI token data to send to the server
    pub token_data: Vec<u8>,
}

#[async_trait]
impl Request for SspiRequest {
    fn packet_type(&self) -> PacketType {
        PacketType::SSPI
    }

    async fn serialize<'a, 'b>(&'a self, writer: &'a mut PacketWriter<'b>) -> TdsResult<()>
    where
        'b: 'a,
    {
        // SSPI message is just the raw token data
        writer.write_async(&self.token_data).await?;
        writer.finalize().await?;
        Ok(())
    }
}

pub(crate) struct LoginResponse {
    // pub model: LoginResponseModel,
    // pub requested_features: Pin<Box<FeaturesRequest>>,
}

impl LoginResponse {
    pub fn new() -> Self {
        LoginResponse {}
    }

    pub(crate) async fn deserialize<T: TdsTokenStreamReader>(
        &self,
        token_stream_reader: &mut T,
        requested_features: FeaturesRequest,
    ) -> TdsResult<LoginResponseModel> {
        let mut response_model = LoginResponseModel::new(requested_features);
        let parser_context = ParserContext::default();
        loop {
            match token_stream_reader
                .receive_token(&parser_context, None, None)
                .await
            {
                Ok(token) => {
                    let token_type = token.token_type();
                    event!(
                        Level::DEBUG,
                        "Received token: {:?} during login response parsing",
                        token_type
                    );

                    match token {
                        Tokens::EnvChange(env_change_token) => {
                            event!(
                                Level::INFO,
                                "Received {:?} during login response parsing.",
                                token_type
                            );
                            // If the redirection is received, then the error will be unwrapped and surfaced
                            // to the caller.
                            response_model.capture_change_property(env_change_token)?;
                        }
                        Tokens::LoginAck(login_ack_token) => {
                            response_model.success_token = Some(login_ack_token);
                        }
                        Tokens::Done(_t) => {
                            // Once we receive a Done token, exit the loop.
                            break;
                        }
                        Tokens::DoneProc(_t) => {
                            // ...
                        }
                        Tokens::DoneInProc(_t) => {
                            // ...
                        }
                        Tokens::Error(error_token) => {
                            event!(
                                Level::ERROR,
                                "Received Error token during login response parsing."
                            );
                            response_model.tds_error = Some(TdsError::new(error_token));
                            // Decide if you want to break here, or keep looping.
                        }
                        Tokens::FeatureExtAck(_t) => {
                            for f in _t.acknowledged_features().iter() {
                                response_model
                                    .features
                                    .set_acknowledged(f.0, f.1.as_slice())?;
                            }
                        }
                        Tokens::FedAuthInfo(fed_auth_info_token) => {
                            response_model.fed_auth_info = Some(fed_auth_info_token);
                            break;
                        }
                        Tokens::Info(_t) => {
                            event!(
                                Level::INFO,
                                "Received {:?} during login response parsing.",
                                token_type
                            );
                        }
                        Tokens::Sspi(sspi_token) => {
                            event!(
                                Level::DEBUG,
                                "SSPI authentication challenge received from server"
                            );
                            response_model.sspi_token = Some(sspi_token);
                            // Break to allow caller to process the SSPI challenge
                            break;
                        }
                        _ => {
                            event!(
                                Level::ERROR,
                                "Received unexpected token during login response parsing. \
                        Check that all tokens from the registry are handled."
                            );
                        }
                    };
                }
                Err(e) => {
                    event!(
                        Level::ERROR,
                        "Failed to receive token during login response parsing. Error: {:?}",
                        e
                    );
                    // if Io error then create the Io error
                    return Err(crate::error::Error::ProtocolError(
                        "Failed to receive token during login response parsing.".to_string(),
                    ));
                }
            }
        }
        Ok(response_model)
    }
}

struct Serializer<'a, 'n, 'context> {
    model: &'a LoginRequestModel<'context>,
    payload_writer: &'a mut PacketWriter<'n>,
    features_request: &'a FeaturesRequest,
    content_next_offset: i32,
    deferred_actions_indicator: Vec<LoginDeferredPayload>,
}

impl<'a, 'n, 'context> Serializer<'a, 'n, 'context> {
    pub fn new(
        model: &'a LoginRequestModel<'context>,
        payload_writer: &'a mut PacketWriter<'n>,
    ) -> Serializer<'a, 'n, 'context> {
        Serializer {
            model,
            payload_writer,
            features_request: &model.features_request,
            content_next_offset: FIXED_LOGIN_RECORD_LENGTH,
            deferred_actions_indicator: Vec::new(),
        }
    }

    /// Calculate the length of the login record.
    /// This includes the fixed length of the login record, the length of the variable length fields,
    /// and the length of the feature extension data.
    fn calculate_login_record_length(&self) -> (i32, i32) {
        let mut login_record_length = FIXED_LOGIN_RECORD_LENGTH;
        login_record_length += self.model.user_input.len_bytes();
        login_record_length += self.model.transport_context.len_bytes();
        login_record_length += 4; // Feature extension offset size.

        // Add SSPI token length if present
        if let Some(sspi_token) = &self.model.sspi_token {
            login_record_length += sspi_token.len() as i32;
        }

        // We write the feature extension at the end of the login record. This is not necessary, but makes reading
        // packets easier. Hence we add the length of the feature extensions data at the end, and save the offset,
        // for the feature extension.
        let feature_extension_offset = login_record_length;
        login_record_length += self.features_request.len_bytes();
        (login_record_length, feature_extension_offset)
    }

    pub(crate) async fn serialize(&mut self) -> TdsResult<()> {
        let (login_record_length, feature_extension_offset) = self.calculate_login_record_length();
        trace!(login_record_length);
        self.payload_writer
            .write_i32_async(login_record_length)
            .await?;
        self.payload_writer
            .write_u32_async(self.model.tds_version.as_u32())
            .await?;

        self.payload_writer
            .write_i32_async(self.model.user_input.packet_size as i32)
            .await?;

        self.payload_writer
            .write_i32_async(self.model.client_prog_ver)
            .await?;

        self.payload_writer
            .write_i32_async(self.model.client_process_id)
            .await?;

        self.payload_writer
            .write_i32_async(self.model.connection_id_deprecated)
            .await?;

        self.payload_writer
            .write_byte_async(self.model.option_flags1.value())
            .await?;

        self.payload_writer
            .write_byte_async(self.model.option_flags2.value())
            .await?;

        self.payload_writer
            .write_byte_async(self.model.type_flags.value())
            .await?;

        self.payload_writer
            .write_byte_async(self.model.option_flags3.value())
            .await?;

        self.payload_writer
            .write_i32_async(self.model.client_time_zone_deprecated)
            .await?;

        self.payload_writer
            .write_i32_async(self.model.client_lcid_deprecated)
            .await?;

        self.write_variable_length_section(feature_extension_offset)
            .await?;

        Ok(())
    }

    async fn write_variable_length_section(
        &mut self,
        feature_extension_offset: i32,
    ) -> TdsResult<()> {
        /* Writing variable-length meta data section
            Fixed-Length Metadata (58 bytes)
            HostNameOffset: 2 bytes
            HostNameLength: 2 bytes
            UserNameOffset: 2 bytes
            UserNameLength: 2 bytes
            PasswordOffset: 2 bytes
            PasswordLength: 2 bytes
            AppNameOffset: 2 bytes
            AppNameLength: 2 bytes
            ServerNameOffset: 2 bytes
            ServerNameLength: 2 bytes
            FeatureExtOffset: 2 bytes
            FeatureExtLength: 2 bytes
            LibraryOffset: 2 bytes
            LibraryLength: 2 bytes
            LanguageOffset: 2 bytes
            LanguageLength: 2 bytes
            DatabaseOffset: 2 bytes
            DatabaseLength: 2 bytes
            ClientID: 6 bytes (fixed length)
            SSPIOffset: 2 bytes
            SSPILength: 2 bytes
            AttachDBFileOffset: 2 bytes
            AttachDBFileLength: 2 bytes
            ChangePasswordOffset: 2 bytes
            ChangePasswordLength: 2 bytes
            cbSSPILong: 4 bytes (DWORD)
        */

        self.write_hostname().await?;
        self.write_username().await?; //
        self.write_password().await?; //
        self.write_app_name().await?;
        self.write_server_name().await?;
        self.write_feature_ext().await?;
        self.write_library_name().await?;
        self.write_language().await?;
        self.write_database().await?;
        self.write_client_id().await?;
        self.write_sspi_short().await?;
        self.write_attach_db_file().await?;
        self.write_change_password().await?;
        self.write_cb_sspi_long().await?;

        // Write all the variable length data.
        for indicator in &self.deferred_actions_indicator {
            match indicator {
                LoginDeferredPayload::HostName => {
                    self.payload_writer
                        .write_string_unicode_async(&self.model.user_input.workstation_id)
                        .await?;
                }
                LoginDeferredPayload::UserName => {
                    self.payload_writer
                        .write_string_unicode_async(&self.model.user_input.user_name)
                        .await?;
                }
                LoginDeferredPayload::Password => {
                    let mut password_utf16_bytes = self
                        .model
                        .user_input
                        .password
                        .encode_utf16()
                        .flat_map(|f| f.to_le_bytes())
                        .collect::<Vec<u8>>();
                    scramble_password(&mut password_utf16_bytes);
                    self.payload_writer
                        .write_async(&password_utf16_bytes)
                        .await?;
                }
                LoginDeferredPayload::AppName => {
                    self.payload_writer
                        .write_string_unicode_async(&self.model.user_input.application_name)
                        .await?;
                }
                LoginDeferredPayload::ServerName => {
                    let server_name = self.model.transport_context.get_login_server_name();
                    // Use get_login_server_name() to get DataSource format (host,port)
                    // This matches SqlClient behavior for redirected connections
                    info!("Login Server name: {}", server_name);
                    self.payload_writer
                        .write_string_unicode_async(server_name.as_str())
                        .await?;
                }
                LoginDeferredPayload::FeatureExtOffset => {
                    self.payload_writer
                        .write_i32_async(feature_extension_offset)
                        .await?;
                }
                LoginDeferredPayload::Library => {
                    self.payload_writer
                        .write_string_unicode_async(&self.model.user_input.library_name)
                        .await?;
                }
                LoginDeferredPayload::Language => {
                    self.payload_writer
                        .write_string_unicode_async(&self.model.user_input.language)
                        .await?;
                }
                LoginDeferredPayload::Database => {
                    self.payload_writer
                        .write_string_unicode_async(&self.model.user_input.database)
                        .await?;
                }
                LoginDeferredPayload::AttachDbFile => {
                    self.payload_writer
                        .write_string_unicode_async(&self.model.user_input.attach_db_file)
                        .await?;
                }
                LoginDeferredPayload::ChangePassword => {
                    let mut password_utf16_bytes = self
                        .model
                        .user_input
                        .change_password
                        .encode_utf16()
                        .flat_map(|f| f.to_le_bytes())
                        .collect::<Vec<u8>>();
                    scramble_password(&mut password_utf16_bytes);
                    self.payload_writer
                        .write_async(&password_utf16_bytes)
                        .await?;
                }
                LoginDeferredPayload::Sspi => {
                    // Write raw SSPI token bytes
                    if let Some(sspi_data) = &self.model.sspi_token {
                        self.payload_writer.write_async(sspi_data).await?;
                    }
                }
            }
        }

        self.write_feature_extension_data().await?;

        debug!(?self.content_next_offset);

        self.payload_writer.finalize().await?;
        Ok(())
    }

    /// Serializes the Feature extension data for each of the requested features.
    async fn write_feature_extension_data(&mut self) -> TdsResult<()> {
        // According to useragent specifications, it should be sent first.
        // By pulling it out of the HashMap explicitly, we guarantee order.
        if let Some(user_agent) = self
            .features_request
            .features
            .get(&FeatureExtension::UserAgent)
            .filter(|f| f.is_requested())
        {
            user_agent.serialize(self.payload_writer).await?;
        }

        for feature in self.features_request.get_requested_features() {
            if feature.feature_identifier() != FeatureExtension::UserAgent {
                feature.serialize(self.payload_writer).await?;
            }
        }
        // Write the terminator
        self.payload_writer.write_byte_async(0xff).await?;
        Ok(())
    }

    /// Writes the value of the hostname of the client to the login packet.
    async fn write_hostname(&mut self) -> TdsResult<()> {
        if self
            .write_metadata(self.model.user_input.workstation_id.len() as i16)
            .await?
        {
            self.deferred_actions_indicator
                .push(LoginDeferredPayload::HostName);
        }
        Ok(())
    }

    /// Writes the value of the username of the client to the login packet if the authentication method is Password.
    async fn write_username(&mut self) -> TdsResult<()> {
        if matches!(
            self.model.user_input.tds_authentication_method,
            TdsAuthenticationMethod::Password
        ) {
            if self
                .write_metadata(self.model.user_input.user_name.len() as i16)
                .await?
            {
                self.deferred_actions_indicator
                    .push(LoginDeferredPayload::UserName);
            }
        } else {
            self.payload_writer.write_i16_async(0).await?;
            self.payload_writer.write_i16_async(0).await?;
        }
        Ok(())
    }

    /// Writes the value of the password of the client to the login packet if the authentication method is Password.
    async fn write_password(&mut self) -> TdsResult<()> {
        if self.model.user_input.tds_authentication_method == TdsAuthenticationMethod::Password {
            if self
                .write_metadata(self.model.user_input.password.len() as i16)
                .await?
            {
                self.deferred_actions_indicator
                    .push(LoginDeferredPayload::Password);
            }
        } else {
            self.payload_writer.write_i16_async(0).await?;
            self.payload_writer.write_i16_async(0).await?;
        }
        Ok(())
    }

    /// Writes the value of the application name provided in the client context to the login packet.
    async fn write_app_name(&mut self) -> TdsResult<()> {
        if self
            .write_metadata(self.model.user_input.application_name.len() as i16)
            .await?
        {
            self.deferred_actions_indicator
                .push(LoginDeferredPayload::AppName);
        }
        Ok(())
    }

    /// Writes the value of the target sql server to the login packet.
    /// Uses get_login_server_name() to get DataSource format (host,port) for TCP connections.
    async fn write_server_name(&mut self) -> TdsResult<()> {
        if self
            .write_metadata(self.model.transport_context.get_login_server_name().len() as i16)
            .await?
        {
            self.deferred_actions_indicator
                .push(LoginDeferredPayload::ServerName);
        }
        Ok(())
    }

    async fn write_feature_ext(&mut self) -> TdsResult<()> {
        // The offset at which to read the feature extension length.
        self.payload_writer
            .write_i16_async(self.content_next_offset as i16)
            .await?;

        // Length of the size of feature extension offset data, which is a DWORD.
        self.payload_writer.write_i16_async(4).await?;

        self.content_next_offset += size_of::<i32>() as i32;

        self.deferred_actions_indicator
            .push(LoginDeferredPayload::FeatureExtOffset);

        Ok(())
    }

    /// Writes the value of the library name to the login packet.
    /// This is also called the Client interface name.
    async fn write_library_name(&mut self) -> TdsResult<()> {
        if self
            .write_metadata(self.model.user_input.library_name.len() as i16)
            .await?
        {
            self.deferred_actions_indicator
                .push(LoginDeferredPayload::Library);
        }
        Ok(())
    }

    async fn write_language(&mut self) -> TdsResult<()> {
        if self
            .write_metadata(self.model.user_input.language.len() as i16)
            .await?
        {
            self.deferred_actions_indicator
                .push(LoginDeferredPayload::Language);
        }
        Ok(())
    }

    /// Writes the value of the database name, which we are connecting to.
    async fn write_database(&mut self) -> TdsResult<()> {
        if self
            .write_metadata(self.model.user_input.database.len() as i16)
            .await?
        {
            self.deferred_actions_indicator
                .push(LoginDeferredPayload::Database);
        }
        Ok(())
    }

    /// Writes the network interface address of the client.
    async fn write_client_id(&mut self) -> TdsResult<()> {
        self.payload_writer
            .write_async(&self.model.client_id.address_bytes)
            .await?;
        Ok(())
    }

    async fn write_sspi_short(&mut self) -> TdsResult<()> {
        // SSPI uses byte length, not character length
        // For tokens <= 65535 bytes, we use the 2-byte length field
        // For tokens > 65535 bytes, we set this to 0xFFFF and use cbSSPILong
        let sspi_len = self.model.sspi_token.as_ref().map_or(0, |t| t.len());

        if sspi_len == 0 {
            // No SSPI token - write offset and zero length
            self.payload_writer
                .write_i16_async(self.content_next_offset as i16)
                .await?;
            self.payload_writer.write_i16_async(0).await?;
            return Ok(());
        }

        // Write offset
        self.payload_writer
            .write_i16_async(self.content_next_offset as i16)
            .await?;

        if sspi_len <= 0xFFFF {
            // Short SSPI token - use 2-byte length
            self.payload_writer.write_u16_async(sspi_len as u16).await?;
        } else {
            // Long SSPI token - use 0xFFFF marker and cbSSPILong
            self.payload_writer.write_u16_async(0xFFFF).await?;
        }

        // Update offset for the SSPI data
        self.content_next_offset += sspi_len as i32;
        self.deferred_actions_indicator
            .push(LoginDeferredPayload::Sspi);

        Ok(())
    }

    async fn write_attach_db_file(&mut self) -> TdsResult<()> {
        if self
            .write_metadata(self.model.user_input.attach_db_file.len() as i16)
            .await?
        {
            self.deferred_actions_indicator
                .push(LoginDeferredPayload::AttachDbFile);
        }
        Ok(())
    }

    async fn write_change_password(&mut self) -> TdsResult<()> {
        if self
            .write_metadata(self.model.user_input.change_password.len() as i16)
            .await?
        {
            self.deferred_actions_indicator
                .push(LoginDeferredPayload::ChangePassword);
        }
        Ok(())
    }

    async fn write_cb_sspi_long(&mut self) -> TdsResult<()> {
        // cbSSPILong is used for SSPI tokens > 65535 bytes
        // When ibSSPI_length is 0xFFFF, this field contains the actual length
        let sspi_len = self.model.sspi_token.as_ref().map_or(0, |t| t.len());

        if sspi_len > 0xFFFF {
            self.payload_writer.write_u32_async(sspi_len as u32).await?;
        } else {
            // Not using long format
            self.payload_writer.write_i32_async(0).await?;
        }
        Ok(())
    }

    async fn write_metadata(&mut self, char_length: i16) -> TdsResult<bool> {
        if char_length == 0 {
            self.payload_writer
                .write_i16_async(self.content_next_offset as i16)
                .await?;
            self.payload_writer.write_i16_async(0).await?;
            return Ok(false);
        }

        self.payload_writer
            .write_i16_async(self.content_next_offset as i16)
            .await?;
        self.payload_writer.write_i16_async(char_length).await?;

        self.content_next_offset += (char_length * 2) as i32;
        Ok(true)
    }
}

#[allow(clippy::manual_rotate)]
fn scramble_password(password_utf16_bytes: &mut [u8]) {
    for b in password_utf16_bytes.iter_mut() {
        *b = (*b >> 4) | (*b << 4); // Swap the nibbles
        *b ^= 0xA5; // XOR with 0xA5
    }
}

/// The deferred actions that are not yet serialized.
/// These values are used to point out the actual payload, that needs to be serialized.
#[derive(Clone, Copy)]
enum LoginDeferredPayload {
    HostName,
    UserName,
    Password,
    AppName,
    ServerName,
    FeatureExtOffset,
    Library,
    Language,
    Database,
    AttachDbFile,
    ChangePassword,
    /// SSPI token data for integrated authentication
    Sspi,
}

/// Trait to calculate the size of the fields for the login records in bytes.
trait SizedLoginItem {
    fn len_bytes(&self) -> i32;
}

impl SizedLoginItem for TransportContext {
    fn len_bytes(&self) -> i32 {
        // Must match what get_login_server_name() returns for consistency
        // with write_server_name() which serializes get_login_server_name()
        self.get_login_server_name().len_bytes()
    }
}

impl SizedLoginItem for String {
    fn len_bytes(&self) -> i32 {
        (self.len() * 2) as i32
    }
}

impl SizedLoginItem for FeaturesRequest {
    fn len_bytes(&self) -> i32 {
        let mut length = 0;
        for feature in self.get_requested_features() {
            length += feature.data_length();
        }

        length += 1; // Feature extension terminator.

        length
    }
}

impl SizedLoginItem for ClientContext {
    fn len_bytes(&self) -> i32 {
        // Do not consider the length of the transport context. That
        // may be overridden by the redirected endpoint.
        let mut client_context_item_length = 0;
        client_context_item_length += self.workstation_id.len_bytes();
        client_context_item_length += self.application_name.len_bytes();
        client_context_item_length += self.library_name.len_bytes();
        client_context_item_length += self.language.len_bytes();
        client_context_item_length += self.database.len_bytes();
        client_context_item_length += self.attach_db_file.len_bytes();

        client_context_item_length += self.calculate_byte_length_for_authentication();
        client_context_item_length
    }
}

impl ClientContext {
    /// Calculate the length of the fields to be persisted for the authentication data.
    /// This would involve the length of the username and password for [TdsAuthenticationMethod::Password].
    /// For TdsAuthenticationMethod::SSPI, the username and password are not sent (they're empty),
    /// and the SSPI token length is calculated separately in LoginRequestModel.
    /// FedAuth or AccessToken authentication is accounted for, in the Feature extension data.
    fn calculate_byte_length_for_authentication(&self) -> i32 {
        let mut length = 0;
        if matches!(
            self.tds_authentication_method,
            TdsAuthenticationMethod::Password
        ) {
            length += self.password.len_bytes();
            length += self.user_name.len_bytes();
        }
        // For SSPI/GSSAPI authentication, username and password fields are empty.
        // The SSPI token is handled separately in LoginRequestModel::calculate_login_record_length().
        length
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Version;
    use crate::message::features::jsonfeature::JsonFeature;
    use crate::message::features::vectorfeature::VectorFeature;
    use crate::token::fed_auth_info::{FedAuthInfoToken, SspiToken};
    use crate::token::login_ack::{LoginAckToken, SqlInterfaceType};
    use crate::token::tokens::{
        EnvChangeContainer, EnvChangeToken, EnvChangeTokenSubType, ErrorToken, SqlCollation,
    };

    // ── FeatureExtension::as_u8 ──

    #[test]
    fn feature_extension_as_u8_all_variants() {
        assert_eq!(FeatureExtension::SRecovery.as_u8(), 0x01);
        assert_eq!(FeatureExtension::FedAuth.as_u8(), 0x02);
        assert_eq!(FeatureExtension::AlwaysEncrypted.as_u8(), 0x04);
        assert_eq!(FeatureExtension::GlobalTransactions.as_u8(), 0x05);
        assert_eq!(FeatureExtension::AzureSqlSupport.as_u8(), 0x08);
        assert_eq!(FeatureExtension::DataClassification.as_u8(), 0x09);
        assert_eq!(FeatureExtension::Utf8Support.as_u8(), 0x0A);
        assert_eq!(FeatureExtension::SqlDnsCaching.as_u8(), 0x0B);
        assert_eq!(FeatureExtension::Json.as_u8(), 0x0D);
        assert_eq!(FeatureExtension::Vector.as_u8(), 0x0E);
        assert_eq!(FeatureExtension::Terminator.as_u8(), 0xFF);
        assert_eq!(FeatureExtension::Unknown(0xAB).as_u8(), 0xAB);
    }

    // ── FeatureExtension::from(u8) ──

    #[test]
    fn feature_extension_from_u8_known_values() {
        assert_eq!(FeatureExtension::from(0x01), FeatureExtension::SRecovery);
        assert_eq!(FeatureExtension::from(0x02), FeatureExtension::FedAuth);
        assert_eq!(
            FeatureExtension::from(0x04),
            FeatureExtension::AlwaysEncrypted
        );
        assert_eq!(
            FeatureExtension::from(0x05),
            FeatureExtension::GlobalTransactions
        );
        assert_eq!(
            FeatureExtension::from(0x08),
            FeatureExtension::AzureSqlSupport
        );
        assert_eq!(
            FeatureExtension::from(0x09),
            FeatureExtension::DataClassification
        );
        assert_eq!(FeatureExtension::from(0x0A), FeatureExtension::Utf8Support);
        assert_eq!(
            FeatureExtension::from(0x0B),
            FeatureExtension::SqlDnsCaching
        );
        assert_eq!(FeatureExtension::from(0x0D), FeatureExtension::Json);
        assert_eq!(FeatureExtension::from(0x0E), FeatureExtension::Vector);
        assert_eq!(FeatureExtension::from(0xFF), FeatureExtension::Terminator);
    }

    #[test]
    fn feature_extension_from_u8_unknown() {
        assert_eq!(
            FeatureExtension::from(0x42),
            FeatureExtension::Unknown(0x42)
        );
    }

    #[test]
    fn feature_extension_roundtrip() {
        for val in [
            0x01u8, 0x02, 0x04, 0x05, 0x08, 0x09, 0x0A, 0x0B, 0x0D, 0x0E, 0xFF, 0x42,
        ] {
            assert_eq!(FeatureExtension::from(val).as_u8(), val);
        }
    }

    // ── Helper to build a FeaturesRequest with Json + Vector ──

    fn make_features_request() -> FeaturesRequest {
        let mut features: HashMap<FeatureExtension, Box<dyn Feature>> = HashMap::new();
        features.insert(FeatureExtension::Json, Box::new(JsonFeature::default()));
        features.insert(FeatureExtension::Vector, Box::new(VectorFeature::default()));
        FeaturesRequest { features }
    }

    // ── FeaturesRequest::features() ──

    #[test]
    fn features_request_features_returns_all() {
        let req = make_features_request();
        assert_eq!(req.features().len(), 2);
    }

    // ── FeaturesRequest::is_acknowledged ──

    #[test]
    fn features_request_is_acknowledged_returns_none_for_unacknowledged() {
        let req = make_features_request();
        assert!(req.is_acknowledged(FeatureExtension::Json).is_none());
    }

    #[test]
    fn features_request_is_acknowledged_returns_some_after_ack() {
        let mut req = make_features_request();
        req.set_acknowledged(FeatureExtension::Json, &[1]).unwrap();
        assert!(req.is_acknowledged(FeatureExtension::Json).is_some());
    }

    #[test]
    fn features_request_is_acknowledged_returns_none_for_missing() {
        let req = make_features_request();
        assert!(req.is_acknowledged(FeatureExtension::SRecovery).is_none());
    }

    // ── FeaturesRequest::set_acknowledged ──

    #[test]
    fn features_request_set_acknowledged_missing_feature_ok() {
        let mut req = make_features_request();
        assert!(
            req.set_acknowledged(FeatureExtension::SRecovery, &[])
                .is_ok()
        );
    }

    // ── FeaturesRequest::get_requested_features / get_acknowledged_features ──

    #[test]
    fn features_request_get_requested_features() {
        let req = make_features_request();
        let requested = req.get_requested_features();
        assert_eq!(requested.len(), 2);
    }

    #[test]
    fn features_request_get_acknowledged_features_initially_empty() {
        let req = make_features_request();
        assert!(req.get_acknowledged_features().is_empty());
    }

    #[test]
    fn features_request_get_acknowledged_features_after_ack() {
        let mut req = make_features_request();
        req.set_acknowledged(FeatureExtension::Json, &[1]).unwrap();
        assert_eq!(req.get_acknowledged_features().len(), 1);
    }

    // ── LoginResponseModel::capture_change_property ──

    fn make_response_model() -> LoginResponseModel {
        LoginResponseModel::new(make_features_request())
    }

    fn env_token(
        sub_type: EnvChangeTokenSubType,
        change_type: EnvChangeContainer,
    ) -> EnvChangeToken {
        EnvChangeToken {
            sub_type,
            change_type,
        }
    }

    #[test]
    fn capture_change_property_database() {
        let mut model = make_response_model();
        let token = env_token(
            EnvChangeTokenSubType::Database,
            ("".to_string(), "master".to_string()).into(),
        );
        model.capture_change_property(token).unwrap();
        assert_eq!(model.change_properties.database.as_deref(), Some("master"));
    }

    #[test]
    fn capture_change_property_language() {
        let mut model = make_response_model();
        let token = env_token(
            EnvChangeTokenSubType::Language,
            ("".to_string(), "us_english".to_string()).into(),
        );
        model.capture_change_property(token).unwrap();
        assert_eq!(
            model.change_properties.language.as_deref(),
            Some("us_english")
        );
    }

    #[test]
    fn capture_change_property_character_set() {
        let mut model = make_response_model();
        let token = env_token(
            EnvChangeTokenSubType::CharacterSet,
            ("".to_string(), "iso_1".to_string()).into(),
        );
        model.capture_change_property(token).unwrap();
        assert_eq!(model.change_properties.char_set.as_deref(), Some("iso_1"));
    }

    #[test]
    fn capture_change_property_string_unknown_subtype_errors() {
        let mut model = make_response_model();
        let token = env_token(
            EnvChangeTokenSubType::UserInstanceName,
            ("val".to_string(), "".to_string()).into(),
        );
        let err = model.capture_change_property(token).unwrap_err();
        assert!(err.to_string().contains("Protocol Error"));
    }

    #[test]
    fn capture_change_property_packet_size() {
        let mut model = make_response_model();
        let token = env_token(EnvChangeTokenSubType::PacketSize, (4096u32, 8192u32).into());
        model.capture_change_property(token).unwrap();
        assert_eq!(model.change_properties.packet_size, 8192);
    }

    #[test]
    fn capture_change_property_uint32_unknown_subtype_errors() {
        let mut model = make_response_model();
        let token = env_token(EnvChangeTokenSubType::BeginTransaction, (1u32, 0u32).into());
        let err = model.capture_change_property(token).unwrap_err();
        assert!(err.to_string().contains("Protocol Error"));
    }

    #[test]
    fn capture_change_property_routing_returns_redirection() {
        let mut model = make_response_model();
        let routing = RoutingInfo {
            protocol: 0,
            server: "redirect.sql.net".to_string(),
            port: 11000,
        };
        let token = env_token(EnvChangeTokenSubType::Routing, (None, Some(routing)).into());
        let err = model.capture_change_property(token).unwrap_err();
        assert!(err.to_string().contains("redirect.sql.net"));
        assert!(model.change_properties.routing_information.is_some());
    }

    #[test]
    fn capture_change_property_bytes_type_errors() {
        let mut model = make_response_model();
        let token = env_token(
            EnvChangeTokenSubType::BeginTransaction,
            (vec![1u8, 2], vec![3u8, 4]).into(),
        );
        let err = model.capture_change_property(token).unwrap_err();
        assert!(err.to_string().contains("Protocol Error"));
    }

    #[test]
    fn capture_change_property_collation() {
        let mut model = make_response_model();
        let token = env_token(
            EnvChangeTokenSubType::SqlCollation,
            EnvChangeContainer::from((None::<SqlCollation>, None::<SqlCollation>)),
        );
        model.capture_change_property(token).unwrap();
        assert!(model.change_properties.database_collation.is_none());
    }

    // ── LoginResponseModel::get_status ──

    #[test]
    fn get_status_rerouting() {
        let mut model = make_response_model();
        model.change_properties.routing_information = Some(RoutingInfo {
            protocol: 0,
            server: "s".to_string(),
            port: 1433,
        });
        assert_eq!(model.get_status(), LoginResponseStatus::Rerouting);
    }

    #[test]
    fn get_status_success() {
        let mut model = make_response_model();
        model.success_token = Some(LoginAckToken {
            interface_type: SqlInterfaceType::TSql,
            tds_version: TdsVersion::V7_4,
            prog_name: "SQL".to_string(),
            prog_version: Version {
                major: 1,
                minor: 0,
                build: 0,
                revision: 0,
            },
        });
        assert_eq!(model.get_status(), LoginResponseStatus::Success);
    }

    #[test]
    fn get_status_error() {
        let mut model = make_response_model();
        model.tds_error = Some(TdsError::new(ErrorToken {
            number: 1,
            state: 0,
            severity: 16,
            message: "err".to_string(),
            server_name: "".to_string(),
            proc_name: "".to_string(),
            line_number: 0,
        }));
        assert_eq!(model.get_status(), LoginResponseStatus::Error);
    }

    #[test]
    fn get_status_waiting_for_fed_auth() {
        let mut model = make_response_model();
        model.fed_auth_info = Some(FedAuthInfoToken {
            spn: "spn".to_string(),
            sts_url: "https://sts".to_string(),
        });
        assert_eq!(model.get_status(), LoginResponseStatus::WaitingForFedAuth);
    }

    #[test]
    fn get_status_waiting_for_sspi() {
        let mut model = make_response_model();
        model.sspi_token = Some(SspiToken {
            data: vec![0x01, 0x02],
        });
        assert_eq!(model.get_status(), LoginResponseStatus::WaitingForSspi);
    }

    #[test]
    fn get_status_fallback_error() {
        let model = make_response_model();
        assert_eq!(model.get_status(), LoginResponseStatus::Error);
    }

    // ── LoginResponseStatus discriminants ──

    #[test]
    fn login_response_status_discriminants() {
        assert_eq!(LoginResponseStatus::NoResponse as u8, 0x00);
        assert_eq!(LoginResponseStatus::Success as u8, 0x01);
        assert_eq!(LoginResponseStatus::Error as u8, 0x02);
        assert_eq!(LoginResponseStatus::WaitingForFedAuth as u8, 0x03);
        assert_eq!(LoginResponseStatus::Rerouting as u8, 0x04);
        assert_eq!(LoginResponseStatus::WaitingForSspi as u8, 0x05);
    }

    // ── get_status priority: routing > success > error > fedauth > sspi ──

    #[test]
    fn get_status_rerouting_takes_precedence_over_success() {
        let mut model = make_response_model();
        model.change_properties.routing_information = Some(RoutingInfo {
            protocol: 0,
            server: "s".to_string(),
            port: 1433,
        });
        model.success_token = Some(LoginAckToken {
            interface_type: SqlInterfaceType::TSql,
            tds_version: TdsVersion::V7_4,
            prog_name: "SQL".to_string(),
            prog_version: Version {
                major: 1,
                minor: 0,
                build: 0,
                revision: 0,
            },
        });
        assert_eq!(model.get_status(), LoginResponseStatus::Rerouting);
    }
}
