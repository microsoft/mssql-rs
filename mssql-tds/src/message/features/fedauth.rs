// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use async_trait::async_trait;
use tracing::event;

use crate::connection::client_context::TdsAuthenticationMethod;
use crate::core::TdsResult;
use crate::io::packet_writer::{PacketWriter, TdsPacketWriter};

use crate::message::login::{Feature, FeatureExtension};

/// Represents the FedAuth feature in the TDS protocol.
///
/// Holds Federated Authentication state for the TDS login sequence.
/// When an access token is provided (as a raw JWT string), it is encoded
/// to UTF-16-LE bytes for the TDS wire format.
#[derive(Clone, Debug)]
pub(crate) struct FedAuthFeature {
    acknowledged: bool,
    tds_authentication_method: TdsAuthenticationMethod,
    access_token_bytes: Option<Vec<u8>>,
    prelogin_has_fedauth_response: bool,
}

impl FedAuthFeature {
    /// Creates a new FedAuthFeature.
    ///
    /// # Arguments
    /// * `tds_authentication_method` - The authentication method being used
    /// * `access_token_base64` - Base64 encoded JWT access token. Encoded to UTF-16-LE for the TDS wire format.
    /// * `prelogin_fedauth_response` - Whether fedauth response was in prelogin
    pub fn new(
        tds_authentication_method: TdsAuthenticationMethod,
        access_token_base64: Option<String>,
        prelogin_fedauth_response: bool,
    ) -> Self {
        // Encode raw JWT to UTF-16-LE for TDS wire format.
        let access_token_bytes = access_token_base64.map(|token| {
            token
                .encode_utf16()
                .flat_map(|u| u.to_le_bytes())
                .collect::<Vec<u8>>()
        });

        Self {
            acknowledged: false,
            tds_authentication_method,
            access_token_bytes,
            prelogin_has_fedauth_response: prelogin_fedauth_response,
        }
    }

    /// Returns the options byte for the FedAuth feature.
    /// The options byte is constructed based on the presence of an access token and the type of authentication method.
    ///
    /// The first bit indicates whether a federated authentication response was received in the pre-login data.
    fn get_options(&self) -> u8 {
        let mut options = 0x00;

        let fedauthlib_securitytoken: u8 = 0x01;
        let fedauthlib_msal: u8 = 0x02;

        if self.tds_authentication_method == TdsAuthenticationMethod::AccessToken {
            options |= fedauthlib_securitytoken << 1;
        } else {
            options |= fedauthlib_msal << 1;
        }
        // Fed auth response received in pre-login data.
        options |= if self.prelogin_has_fedauth_response {
            0x01
        } else {
            0x00
        };
        options
    }

    /// Returns the workflow identifier based on the TDS authentication method.
    /// This is used by the server to determine the authentication flow.
    /// The workflow identifier is a byte that indicates the type of authentication being used.
    /// The mapping of authentication methods to workflow identifiers is as follows:
    /// Some of the workflow identifiers are re-used.
    fn get_work_flow_identifier(&self) -> TdsResult<u8> {
        let active_directory_password: u8 = 0x01;
        let active_directory_integrated: u8 = 0x02;
        let active_directory_interactive: u8 = 0x03;
        let active_directory_service_principal: u8 = 0x01; // Using the Password byte as that is the closest we have
        let active_directory_device_code_flow: u8 = 0x03; // Using the Interactive byte as that is the closest we have
        let active_directory_managed_identity: u8 = 0x03; // Using the Interactive byte as that's supported for Identity based authentication
        let _active_directory_default: u8 = 0x03; // Using the Interactive byte as that is the closest we have to non-password based authentication modes
        let active_directory_token_credential: u8 = 0x03; // Using the Interactive byte as that is the closest we have to non-password based authentication modes
        let active_directory_workload_identity: u8 = 0x03; // Using the Interactive byte as that's supported for Identity based authentication

        match self.tds_authentication_method {
            TdsAuthenticationMethod::ActiveDirectoryInteractive => Ok(active_directory_interactive),
            TdsAuthenticationMethod::ActiveDirectoryIntegrated => Ok(active_directory_integrated),
            TdsAuthenticationMethod::ActiveDirectoryPassword => Ok(active_directory_password),
            TdsAuthenticationMethod::ActiveDirectoryServicePrincipal => {
                Ok(active_directory_service_principal)
            }
            TdsAuthenticationMethod::ActiveDirectoryDeviceCodeFlow => {
                Ok(active_directory_device_code_flow)
            }
            TdsAuthenticationMethod::ActiveDirectoryManagedIdentity => {
                Ok(active_directory_managed_identity)
            }
            TdsAuthenticationMethod::ActiveDirectoryWorkloadIdentity => {
                Ok(active_directory_workload_identity)
            }
            TdsAuthenticationMethod::ActiveDirectoryDefault => {
                Ok(active_directory_token_credential)
            }
            _ => Err(crate::error::Error::ProtocolError(format!(
                "Unsupported authentication method {:?} used with FedAuth feature",
                self.tds_authentication_method
            ))),
        }
    }

    /// Returns the length of the payload for the FedAuth feature.
    /// The payload length is calculated based on the presence of an access token.
    /// If an access token is present, the length includes the size of the token.
    /// If no access token is present, the length is 2.
    fn get_payload_length(&self) -> TdsResult<i32> {
        let len = if let Some(bytes) = &self.access_token_bytes {
            1 + size_of::<i32>() + bytes.len()
        } else {
            2
        };
        i32::try_from(len).map_err(|_| {
            crate::error::Error::ProtocolError(format!(
                "FedAuth payload length {len} exceeds i32 range"
            ))
        })
    }
}

#[async_trait]
impl Feature for FedAuthFeature {
    fn feature_identifier(&self) -> FeatureExtension {
        FeatureExtension::FedAuth
    }

    fn data_length(&self) -> i32 {
        // get_payload_length only fails if token > 2GB, which cannot happen in practice
        #[allow(clippy::unwrap_used)]
        let data_length = self.get_payload_length().unwrap();
        let base_length = size_of::<u8>() + size_of::<i32>();
        data_length + base_length as i32
    }

    fn is_requested(&self) -> bool {
        self.tds_authentication_method != TdsAuthenticationMethod::Password
            && self.tds_authentication_method != TdsAuthenticationMethod::SSPI
    }

    async fn serialize(&self, packet_writer: &mut PacketWriter) -> TdsResult<()> {
        packet_writer
            .write_byte_async(self.feature_identifier().as_u8())
            .await?;
        packet_writer
            .write_i32_async(self.get_payload_length()?)
            .await?;

        packet_writer.write_byte_async(self.get_options()).await?;
        if let Some(bytes) = &self.access_token_bytes {
            packet_writer.write_i32_async(bytes.len() as i32).await?;
            packet_writer.write_async(bytes).await?;
        } else {
            let workflow_identifier = self.get_work_flow_identifier()?;
            packet_writer.write_byte_async(workflow_identifier).await?;
        }
        Ok(())
    }

    fn deserialize(&mut self, data: &[u8]) -> TdsResult<()> {
        if !data.is_empty() {
            event!(
                tracing::Level::WARN,
                "FedAuth feature deserialize received non-empty data of length {}, expected empty. Ignoring.",
                data.len()
            );
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
mod unittests {
    use super::*;

    #[test]
    fn test_get_options_with_access_token() {
        let feature = FedAuthFeature::new(
            TdsAuthenticationMethod::AccessToken,
            Some("token".to_string()),
            true,
        );
        assert_eq!(feature.get_options(), 0x03);
    }

    #[test]
    fn test_get_options_without_access_token() {
        let feature =
            FedAuthFeature::new(TdsAuthenticationMethod::ActiveDirectoryPassword, None, true);
        assert_eq!(feature.get_options(), 0x05);
    }

    #[test]
    fn test_get_work_flow_identifier() {
        let feature = FedAuthFeature::new(
            TdsAuthenticationMethod::ActiveDirectoryInteractive,
            None,
            false,
        );
        assert_eq!(feature.get_work_flow_identifier().unwrap(), 0x03);
    }

    #[test]
    fn test_get_work_flow_identifier_unsupported() {
        let feature = FedAuthFeature::new(TdsAuthenticationMethod::Password, None, false);
        assert!(feature.get_work_flow_identifier().is_err());
    }

    #[test]
    fn test_feature_identifier() {
        let feature = FedAuthFeature::new(
            TdsAuthenticationMethod::ActiveDirectoryPassword,
            None,
            false,
        );
        assert_eq!(feature.feature_identifier(), FeatureExtension::FedAuth);
    }

    #[test]
    fn test_data_length_without_access_token() {
        let feature = FedAuthFeature::new(
            TdsAuthenticationMethod::ActiveDirectoryPassword,
            None,
            false,
        );
        assert_eq!(feature.data_length(), 2 + 1 + 4);
    }

    #[test]
    fn test_is_requested() {
        let feature = FedAuthFeature::new(TdsAuthenticationMethod::Password, None, false);
        assert!(!feature.is_requested());

        let feature = FedAuthFeature::new(
            TdsAuthenticationMethod::ActiveDirectoryInteractive,
            None,
            false,
        );
        assert!(feature.is_requested());
    }

    #[test]
    fn test_is_acknowledged() {
        let mut feature = FedAuthFeature::new(
            TdsAuthenticationMethod::ActiveDirectoryPassword,
            None,
            false,
        );
        assert!(!feature.is_acknowledged());
        feature.set_acknowledged(true);
        assert!(feature.is_acknowledged());
    }
}
