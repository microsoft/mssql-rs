// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::connection::client_context::{ClientContext, TransportContext};
use crate::connection::session_recovery::SessionRecoveryData;
use crate::core::{EncryptionSetting, NegotiatedEncryptionSetting, TdsResult, Version};
use crate::error::Error;
use crate::handler::sspi_handler::SspiAuthHandler;
use crate::io::packet_reader::TdsPacketReader;
use crate::io::reader_writer::NetworkReaderWriter;
use crate::io::token_stream::TdsTokenStreamReader;
use crate::message::login::{
    EnvChangeProperties, Feature, FeatureExtension, FeaturesRequest, FedAuthTokenRequest,
    LoginRequest, LoginRequestModel, LoginResponse, LoginResponseModel, LoginResponseStatus,
    SspiRequest,
};
use crate::message::login_options::TdsVersion;
use crate::message::messages::Request;
use crate::message::prelogin::{
    EncryptionType, PreloginRequest, PreloginRequestModel, PreloginResponse,
};
use crate::token::login_ack::LoginAckToken;
use crate::token::tokens::SqlCollation;
use tracing::{debug, warn};
use uuid::Uuid;

pub(crate) struct HandlerFactory {
    pub(crate) context: ClientContext,
    /// Recovery data for session reconnection, passed separately from ClientContext
    /// since it is internal-only state not meant for library consumers.
    pub(crate) recovery_data: Option<Box<SessionRecoveryData>>,
}

impl HandlerFactory {
    pub(crate) fn prelogin_handler(&self) -> PreloginHandler<'_> {
        PreloginHandler { factory: self }
    }

    pub(crate) fn login_handler(&self, prelogin_fedauth_supported: bool) -> LoginHandler<'_> {
        LoginHandler {
            factory: self,
            prelogin_fedauth_supported,
        }
    }

    pub(crate) fn session_handler<'a>(
        &'a self,
        transport_context: &'a TransportContext,
    ) -> SessionHandler<'a, 'a> {
        SessionHandler {
            factory: self,
            transport_context,
        }
    }

    fn create_login_request<'a, 'b>(
        &'a self,
        prelogin_fedauth_supported: bool,
        transport_context: &'b TransportContext,
    ) -> LoginRequest<'a>
    where
        'b: 'a,
    {
        let model = self.create_login_model(prelogin_fedauth_supported, transport_context);
        LoginRequest { model }
    }

    fn create_login_model<'a, 'b>(
        &'a self,
        prelogin_fedauth_supported: bool,
        transport_context: &'b TransportContext,
    ) -> LoginRequestModel<'a>
    where
        'b: 'a,
    {
        LoginRequestModel::from_context(
            &self.context,
            prelogin_fedauth_supported,
            transport_context,
            self.recovery_data.as_deref(),
        )
    }

    fn create_login_response(&self) -> LoginResponse {
        LoginResponse::new()
    }
}

// The settings that can be negotiated during and after the login process as well.
#[derive(Debug)]
pub(crate) struct NegotiatedSettings {
    pub session_settings: SessionSettings,
    pub database_collation: SqlCollation,
    #[allow(dead_code)] // populated during login, consumed by future env-change tracking
    pub language: String,
    #[allow(dead_code)] // populated during login, consumed by future env-change tracking
    pub database: String,
    #[allow(dead_code)] // populated during login, consumed by future env-change tracking
    pub char_set: Option<String>,
    /// TDS version from LoginAckToken, captured for session recovery validation.
    pub login_ack_tds_version: Option<TdsVersion>,
    /// Server program version from LoginAckToken, captured for session recovery validation.
    pub login_ack_server_version: Option<Version>,
}

impl NegotiatedSettings {
    fn new(
        session_settings: SessionSettings,
        database_collation: SqlCollation,
        language: String,
        database: String,
        char_set: Option<String>,
        login_ack_tds_version: Option<TdsVersion>,
        login_ack_server_version: Option<Version>,
    ) -> Self {
        NegotiatedSettings {
            session_settings,
            database_collation,
            language,
            database,
            char_set,
            login_ack_tds_version,
            login_ack_server_version,
        }
    }

    /// Check if session recovery was acknowledged by the server in FEATUREEXTACK.
    pub(crate) fn is_session_recovery_acknowledged(&self) -> bool {
        self.session_settings
            .supported_features
            .iter()
            .any(|f| f.feature_identifier() == FeatureExtension::SRecovery && f.is_acknowledged())
    }
}

// The settings of the session that are negotiated during the login process. They dont change after login.
#[derive(Debug)]
pub(crate) struct SessionSettings {
    pub packet_size: u32,
    #[allow(dead_code)] // populated during login, consumed by future session introspection
    pub user_name: String,
    pub(crate) supported_features: Vec<Box<dyn Feature>>,
    #[allow(dead_code)] // populated during login, consumed by future MARS support
    pub(crate) mars_enabled: bool,
    #[allow(dead_code)] // populated during login, consumed by future auth flow
    pub pre_login_has_fedauth_supported: bool,
    #[allow(dead_code)] // populated during login, consumed by future encryption tracking
    pub negotiated_encryption_settings: NegotiatedEncryptionSetting,
}

impl SessionSettings {
    // Note that this destructively consumes the features list.
    fn new(
        context: &ClientContext,
        pre_login_has_fedauth_supported: bool,
        packet_size: u32,
        negotiated_encryption_settings: NegotiatedEncryptionSetting,
        features: &mut Vec<Box<dyn Feature>>,
    ) -> Self {
        let mut result = SessionSettings {
            packet_size,
            user_name: context.user_name.clone(),
            supported_features: vec![],
            mars_enabled: context.mars_enabled,
            pre_login_has_fedauth_supported,
            negotiated_encryption_settings,
        };
        result.supported_features.append(features);
        result
    }
}

// Helper function for fuzzing and tests to create test settings
#[cfg(any(fuzzing, test))]
pub(crate) fn create_test_negotiated_settings_internal() -> NegotiatedSettings {
    let session_settings = SessionSettings {
        packet_size: 4096,
        user_name: "test".to_string(),
        supported_features: Vec::new(),
        mars_enabled: false,
        pre_login_has_fedauth_supported: false,
        negotiated_encryption_settings: NegotiatedEncryptionSetting::NoEncryption,
    };

    NegotiatedSettings {
        session_settings,
        database_collation: SqlCollation {
            info: 0,
            lcid_language_id: 0x0409,
            col_flags: 0,
            sort_id: 0,
        },
        language: "us_english".to_string(),
        database: "master".to_string(),
        char_set: None,
        login_ack_tds_version: None,
        login_ack_server_version: None,
    }
}

pub(crate) struct SessionHandler<'a, 'b> {
    pub(crate) factory: &'a HandlerFactory,
    pub(crate) transport_context: &'b TransportContext,
}

impl<'a, 'b> SessionHandler<'a, 'b> {
    pub(crate) async fn execute<T: NetworkReaderWriter + TdsTokenStreamReader + TdsPacketReader>(
        &mut self,
        reader_writer: &mut T,
    ) -> TdsResult<NegotiatedSettings> {
        let pre_login_result = self.get_pre_login_result(reader_writer).await?;
        self.validate_prelogin_result(&pre_login_result)?;

        // Note: This must happen before login because the login process can use the negotiated
        // encryption setting.
        reader_writer.notify_encryption_setting_change(pre_login_result.encryption_setting);

        let mut login_result = self
            .get_login_result(reader_writer, pre_login_result.is_fed_auth_supported)
            .await?;
        self.validate_login_result(&login_result)?;

        let negotiated_settings =
            self.infer_negotiated_settings(&pre_login_result, &mut login_result)?;
        reader_writer.notify_session_setting_change(&negotiated_settings.session_settings);
        Ok(negotiated_settings)
    }

    async fn get_pre_login_result<T: NetworkReaderWriter + TdsPacketReader>(
        &self,
        reader_writer: &mut T,
    ) -> TdsResult<PreloginResult> {
        let result = self
            .factory
            .prelogin_handler()
            .execute(reader_writer)
            .await?;
        Ok(result)
    }

    fn validate_prelogin_result(&self, _result: &PreloginResult) -> TdsResult<()> {
        // TBDif sever does not support fed auth and client expects fed auth then throw an exception.
        Ok(())
    }

    fn validate_login_result(&self, result: &LoginResult) -> TdsResult<()> {
        // Check if login succeeded
        if result.status == LoginResponseStatus::Error {
            return Err(Error::ProtocolError(
                "Login failed: Server did not send login acknowledgement".to_string(),
            ));
        }
        Ok(())
    }

    fn infer_negotiated_settings(
        &self,
        prelogin_result: &PreloginResult,
        login_result: &mut LoginResult,
    ) -> TdsResult<NegotiatedSettings> {
        let change_props = &login_result.change_properties;
        let packet_size = change_props.packet_size as u32;
        let session_settings = SessionSettings::new(
            &self.factory.context,
            prelogin_result.is_fed_auth_supported,
            packet_size,
            prelogin_result.encryption_setting,
            &mut login_result.supported_features,
        );

        let database_collation = match change_props.database_collation {
            Some(ref collation) => *collation,
            None => {
                return Err(Error::ProtocolError(
                    "Database collation missing after login. Server must send database collation in EnvChange token.".to_string()
                ));
            }
        };

        let database = match change_props.database {
            Some(ref db) => db.clone(),
            None => {
                return Err(Error::ProtocolError(
                    "Database name missing after login. Server must send database name in EnvChange token.".to_string()
                ));
            }
        };

        let (login_ack_tds_version, login_ack_server_version) = match &login_result.login_ack {
            Some(ack) => (Some(ack.tds_version), Some(ack.prog_version)),
            None => (None, None),
        };

        Ok(NegotiatedSettings::new(
            session_settings,
            database_collation,
            "".to_string(),
            database,
            change_props.char_set.clone(),
            login_ack_tds_version,
            login_ack_server_version,
        ))
    }

    async fn get_login_result<T: TdsTokenStreamReader + NetworkReaderWriter>(
        &mut self,
        reader_writer: &mut T,
        prelogin_fedauth_supported: bool,
    ) -> TdsResult<LoginResult> {
        self.factory
            .login_handler(prelogin_fedauth_supported)
            .execute(reader_writer, self.transport_context)
            .await
    }
}

struct PreloginResult {
    encryption_setting: NegotiatedEncryptionSetting,
    is_fed_auth_supported: bool,
}

impl PreloginResult {}

pub(crate) struct PreloginHandler<'a> {
    factory: &'a HandlerFactory,
}

impl PreloginHandler<'_> {
    async fn execute<T: NetworkReaderWriter + TdsPacketReader>(
        &self,
        reader_writer: &mut T,
    ) -> TdsResult<PreloginResult> {
        // Create the request.
        let request_model = PreloginRequestModel::new(
            Uuid::new_v4(),
            Option::from(self.factory.context.mars_enabled),
            Option::from(self.factory.context.encryption_options.mode),
            Option::from(self.factory.context.database_instance.as_str()),
        );
        let prelogin_request = PreloginRequest {
            model: &request_model,
        };

        // Serialize it. Note that the login process uses a timeout at a higher level than the request.
        let mut packet_writer = prelogin_request.create_packet_writer(reader_writer, None, None);
        prelogin_request.serialize(&mut packet_writer).await?;

        // Return result (which contains data model).
        let response = PreloginResponse {};

        let response_model = &response.deserialize(reader_writer).await?;
        if request_model.mars_enabled && !response_model.mars_enabled.unwrap_or(false) {
            return Err(Error::ProtocolError(
                "Server does not support MARS (Multiple Active Result Sets)".to_string(),
            ));
        }

        if !response_model.dbinstance_valid.unwrap() {
            // Non-fatal behaviour.
            warn!("Database instance validation failed");
        }

        if request_model.encryption_setting == EncryptionSetting::Strict {
            // If strict is used, the user is using TDS 8. The server's encryption response is
            // unused and the stream is already (and stays) encrypted.
            return Ok(PreloginResult {
                encryption_setting: NegotiatedEncryptionSetting::Strict,
                is_fed_auth_supported: response_model.federated_auth_supported,
            });
        }

        match &response_model.encryption {
            EncryptionType::Off => {
                // The _only_ way the server would have sent Off would be if the
                // client asked for it to be off. If the server supports
                // encryption it will return On and if not returns Unsupported.
                if request_model.encryption_setting == EncryptionSetting::PreferOff {
                    Ok(PreloginResult {
                        encryption_setting: NegotiatedEncryptionSetting::LoginOnly,
                        is_fed_auth_supported: response_model.federated_auth_supported,
                    })
                } else {
                    Err(Error::ProtocolError(format!(
                        "Server disallowed encryption but client requires it (client: {:?}, server: Off)",
                        request_model.encryption_setting
                    )))
                }
            }
            EncryptionType::NotSupported => {
                if request_model.encryption_setting == EncryptionSetting::PreferOff {
                    Ok(PreloginResult {
                        encryption_setting: NegotiatedEncryptionSetting::NoEncryption,
                        is_fed_auth_supported: response_model.federated_auth_supported,
                    })
                } else {
                    Err(Error::ProtocolError(format!(
                        "Server does not support encryption but client requires it (client: {:?}, server: NotSupported)",
                        request_model.encryption_setting
                    )))
                }
            }
            _ => Ok(PreloginResult {
                encryption_setting: NegotiatedEncryptionSetting::Mandatory,
                is_fed_auth_supported: response_model.federated_auth_supported,
            }),
        }
    }
}

struct LoginResult {
    supported_features: Vec<Box<dyn Feature>>,
    change_properties: EnvChangeProperties,
    status: LoginResponseStatus,
    login_ack: Option<LoginAckToken>,
}

pub struct LoginHandler<'a> {
    factory: &'a HandlerFactory,
    prelogin_fedauth_supported: bool,
}

impl LoginHandler<'_> {
    async fn execute<T: TdsTokenStreamReader + NetworkReaderWriter>(
        &self,
        reader_writer: &mut T,
        transport_context: &TransportContext,
    ) -> TdsResult<LoginResult> {
        let encryption = reader_writer.get_encryption_setting();
        if encryption != NegotiatedEncryptionSetting::Strict
            && encryption != NegotiatedEncryptionSetting::NoEncryption
        {
            // Note: We should not toggle encryption on if Strict is used because it is already,
            // and we shouldn't alter the streams.
            reader_writer.enable_ssl().await?;
        }

        let (request_model, mut sspi_handler) = self
            .send_login7_request(reader_writer, transport_context)
            .await?;
        let requested_features = request_model.features_request;
        let mut login_response = self
            .get_login_response(reader_writer, requested_features.clone())
            .await?;

        // Handle SSPI challenge-response loop for integrated authentication
        while login_response.get_status() == LoginResponseStatus::WaitingForSspi {
            let sspi_challenge = login_response.sspi_token.as_ref().ok_or_else(|| {
                Error::ProtocolError(
                    "Login response status is WaitingForSspi but no sspi_token present".to_string(),
                )
            })?;

            let handler = sspi_handler.as_mut().ok_or_else(|| {
                Error::ProtocolError(
                    "Received SSPI challenge but no SSPI handler available".to_string(),
                )
            })?;

            debug!(
                "Processing SSPI challenge ({} bytes)",
                sspi_challenge.data.len()
            );

            // Process the challenge and generate response
            let response_opt = handler.process_challenge(&sspi_challenge.data)?;
            match response_opt {
                Some(response_token) => {
                    debug!("Sending SSPI response ({} bytes)", response_token.len());

                    // Send SSPI response as packet type 0x11
                    let sspi_request = SspiRequest {
                        token_data: response_token,
                    };
                    let mut packet_writer =
                        sspi_request.create_packet_writer(reader_writer, None, None);
                    sspi_request.serialize(&mut packet_writer).await?;

                    // Get next response from server after sending our response
                    login_response = self
                        .get_login_response(reader_writer, requested_features.clone())
                        .await?;
                }
                None => {
                    // No response token needed from GSSAPI, but we still need to read
                    // the server's final response (LoginAck or next challenge)
                    debug!(
                        "SSPI authentication complete, no response needed - reading final server response"
                    );

                    // Read the final response from server
                    login_response = self
                        .get_login_response(reader_writer, requested_features.clone())
                        .await?;
                    // The final response was read - the while loop will now exit since status is no longer WaitingForSspi
                }
            }
        }

        // Handle the case where federated authentication was requested, and now we have to respond.
        login_response = if login_response.get_status() == LoginResponseStatus::WaitingForFedAuth {
            let fed_auth_info = match &login_response.fed_auth_info {
                Some(fed_auth_info) => fed_auth_info,
                None => {
                    return Err(Error::ProtocolError(
                        "Login response status is WaitingForFedAuth but no fed_auth_info present. Protocol error.".to_string()
                    ));
                }
            };
            let context = &self.factory.context;
            let entra_id_token_factory = context.entra_id_token_factory()?;

            let token = entra_id_token_factory
                .create_token(
                    fed_auth_info.spn.clone(),
                    fed_auth_info.sts_url.clone(),
                    context.tds_authentication_method.clone(),
                )
                .await?;
            let fed_auth_request = FedAuthTokenRequest {
                access_token_bytes: token,
            };

            let mut packet_writer =
                fed_auth_request.create_packet_writer(reader_writer, None, None);
            fed_auth_request.serialize(&mut packet_writer).await?;
            self.get_login_response(reader_writer, requested_features)
                .await?
        } else {
            login_response
        };

        let response_status = login_response.get_status();

        Ok(LoginResult {
            supported_features: vec![],
            change_properties: login_response.change_properties,
            status: response_status,
            login_ack: login_response.success_token,
        })
    }

    async fn send_login7_request<'a, 'b>(
        &'a self,
        reader_writer: &mut impl NetworkReaderWriter,
        transport_context: &'b TransportContext,
    ) -> TdsResult<(LoginRequestModel<'a>, Option<SspiAuthHandler>)>
    where
        'b: 'a,
    {
        let context = &self.factory.context;
        let is_integrated_security = context.integrated_security();

        // If using integrated security, create SSPI handler and get initial token
        let (sspi_token, sspi_handler) = if is_integrated_security {
            let config = context.integrated_auth_config();
            let server = transport_context.get_server_name();
            let port = transport_context.get_port();

            debug!(
                "Creating SSPI handler for integrated authentication to {}:{}",
                server, port
            );

            let mut handler = SspiAuthHandler::new(&config, &server, port)?;
            let initial_token = handler.get_initial_token()?;

            debug!(
                "Generated initial SSPI token ({} bytes) using {}",
                initial_token.len(),
                handler.package_name()
            );

            (Some(initial_token), Some(handler))
        } else {
            (None, None)
        };

        // Create the login request, optionally with SSPI token
        let request = if let Some(token) = sspi_token {
            LoginRequest {
                model: LoginRequestModel::from_context_with_sspi(
                    context,
                    self.prelogin_fedauth_supported,
                    transport_context,
                    token,
                    self.factory.recovery_data.as_deref(),
                ),
            }
        } else {
            self.factory
                .create_login_request(self.prelogin_fedauth_supported, transport_context)
        };

        // Note that the login process uses a timeout at a higher level than the request.
        let mut packet_writer = request.create_packet_writer(reader_writer, None, None);
        request.serialize(&mut packet_writer).await?;

        Ok((request.model, sspi_handler))
    }

    async fn get_login_response<T: TdsTokenStreamReader>(
        &self,
        reader_writer: &mut T,
        requested_features: FeaturesRequest,
    ) -> TdsResult<LoginResponseModel> {
        let response = self.factory.create_login_response();
        let response_model = response
            .deserialize(reader_writer, requested_features)
            .await?;
        if response_model.tds_error.is_some() {
            let tds_error = response_model.tds_error.unwrap();
            Err(Error::from_sql_error(crate::error::SqlErrorInfo {
                message: tds_error.get_message(),
                state: tds_error.error_token.state,
                class: tds_error.error_token.severity as i32,
                number: tds_error.error_token.number,
                server_name: None,
                proc_name: None,
                line_number: None,
            }))
        } else {
            Ok(response_model)
        }
    }
}
