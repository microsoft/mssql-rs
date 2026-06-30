// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Small, network-free helpers shared by the built-in Entra ID factories:
//! scope normalization, STS URL parsing, and FEDAUTHTOKEN wire encoding.

/// An STS (Security Token Service) authority parsed from the server-provided
/// `sts_url` in the `FEDAUTHINFO` token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StsUrl {
    /// The authority host, passed through verbatim to
    /// `TokenCredentialOptions::set_authority_host`. May include a tenant path
    /// segment; `azure_identity` rebuilds the token endpoint from the authority
    /// origin plus the explicit tenant, so a trailing path is harmless.
    pub authority_host: String,
    /// The tenant identifier (GUID or domain) taken from the first path segment,
    /// when present. Required by the service-principal flow.
    pub tenant_id: Option<String>,
}

/// Normalizes an SPN/resource into an OAuth2 scope ending in `/.default`.
///
/// Azure SQL hands back a resource such as `https://database.windows.net/`; the
/// credential APIs expect a scope like `https://database.windows.net/.default`.
pub(crate) fn normalize_scope(spn: &str) -> String {
    if spn.ends_with("/.default") {
        spn.to_string()
    } else if spn.ends_with('/') {
        format!("{spn}.default")
    } else {
        format!("{spn}/.default")
    }
}

/// Parses the server-provided `sts_url` into an [`StsUrl`].
///
/// `https://login.microsoftonline.com/<tenant>` yields `tenant_id = <tenant>`;
/// a bare `https://login.microsoftonline.com/` yields `tenant_id = None`.
pub(crate) fn parse_sts_url(sts_url: &str) -> StsUrl {
    let trimmed = sts_url.trim();
    let after_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    let tenant_id = after_scheme
        .split('/')
        .nth(1)
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(str::to_string);
    StsUrl {
        authority_host: trimmed.to_string(),
        tenant_id,
    }
}

/// Encodes a JWT into the little-endian UTF-16 byte sequence expected in the
/// `FEDAUTHTOKEN` payload.
pub(crate) fn encode_jwt_utf16le(jwt: &str) -> Vec<u8> {
    jwt.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_scope_appends_default() {
        assert_eq!(
            normalize_scope("https://database.windows.net"),
            "https://database.windows.net/.default"
        );
        assert_eq!(
            normalize_scope("https://database.windows.net/"),
            "https://database.windows.net/.default"
        );
        assert_eq!(
            normalize_scope("https://database.windows.net/.default"),
            "https://database.windows.net/.default"
        );
    }

    #[test]
    fn parse_sts_url_extracts_tenant() {
        let parsed = parse_sts_url("https://login.microsoftonline.com/contoso-tenant-id");
        assert_eq!(
            parsed.authority_host,
            "https://login.microsoftonline.com/contoso-tenant-id"
        );
        assert_eq!(parsed.tenant_id.as_deref(), Some("contoso-tenant-id"));
    }

    #[test]
    fn parse_sts_url_handles_trailing_slash_and_missing_tenant() {
        assert_eq!(parse_sts_url("https://login.windows.net/").tenant_id, None);
        assert_eq!(parse_sts_url("https://login.windows.net").tenant_id, None);
        let parsed = parse_sts_url("https://login.windows.net/my-tenant/");
        assert_eq!(parsed.tenant_id.as_deref(), Some("my-tenant"));
    }

    #[test]
    fn encode_jwt_utf16le_is_little_endian() {
        assert_eq!(encode_jwt_utf16le("AB"), vec![0x41, 0x00, 0x42, 0x00]);
        assert!(encode_jwt_utf16le("").is_empty());
    }
}
