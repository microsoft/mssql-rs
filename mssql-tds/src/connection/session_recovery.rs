use crate::connection::client_context::ClientContext;
use crate::connection::execution_context::ExecutionContext;
use crate::core::{NegotiatedEncryptionSetting, TdsResult, Version};
use crate::message::login_options::TdsVersion;
use crate::token::tokens::{SessionStateToken, SqlCollation};

/// Manages session recovery state for idle connection resiliency.
///
/// Wraps a [`SessionStateTable`] and tracks whether session recovery was
/// negotiated with the server. Used by `TdsClient` to process SESSIONSTATE
/// tokens during normal operation and to determine recovery eligibility.
pub(crate) struct RecoveryContext {
    /// Whether the server acknowledged session recovery in FEATUREEXTACK.
    pub session_recovery_negotiated: bool,
    /// Accumulated session state for reconnection.
    /// Boxed to avoid ~16KB on the stack during construction (two 256-element arrays).
    pub session_state_table: Box<SessionStateTable>,
    /// Clone of the original connection configuration, needed for reconnection.
    /// Boxed to avoid inflating async future state machines (ClientContext is ~17KB).
    pub client_context: Option<Box<ClientContext>>,
    /// TDS version from the initial LoginAckToken.
    pub original_tds_version: Option<TdsVersion>,
    /// Server program version from the initial LoginAckToken.
    pub original_server_version: Option<Version>,
    /// Negotiated encryption level from the initial connection.
    pub original_encryption_level: Option<NegotiatedEncryptionSetting>,
    /// Whether MARS was enabled on the initial connection.
    pub original_mars_enabled: bool,
    /// Number of successful recovery attempts performed.
    pub recovery_count: u32,
}

impl std::fmt::Debug for RecoveryContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryContext")
            .field(
                "session_recovery_negotiated",
                &self.session_recovery_negotiated,
            )
            .field("session_state_table", &self.session_state_table)
            .field(
                "client_context",
                &self.client_context.as_ref().map(|_| "<ClientContext>"),
            )
            .field("original_tds_version", &self.original_tds_version)
            .field("original_server_version", &self.original_server_version)
            .field("original_encryption_level", &self.original_encryption_level)
            .field("original_mars_enabled", &self.original_mars_enabled)
            .field("recovery_count", &self.recovery_count)
            .finish()
    }
}

impl RecoveryContext {
    pub fn new() -> Self {
        Self {
            session_recovery_negotiated: false,
            session_state_table: Box::new(SessionStateTable::new()),
            client_context: None,
            original_tds_version: None,
            original_server_version: None,
            original_encryption_level: None,
            original_mars_enabled: false,
            recovery_count: 0,
        }
    }

    /// Initialize recovery context with connection-time settings.
    /// Called after a successful login to capture the original connection parameters
    /// needed for reconnection validation and orchestration, and to seed the
    /// baseline session state a reconnect's LOGIN7 must replay.
    pub fn initialize(
        &mut self,
        client_context: ClientContext,
        negotiated_settings: &crate::handler::handler_factory::NegotiatedSettings,
        login_session_state_tokens: &[SessionStateToken],
    ) {
        self.client_context = Some(Box::new(client_context));
        self.original_tds_version = negotiated_settings.login_ack_tds_version;
        self.original_server_version = negotiated_settings.login_ack_server_version;
        self.original_encryption_level = Some(
            negotiated_settings
                .session_settings
                .negotiated_encryption_settings,
        );
        self.original_mars_enabled = negotiated_settings.session_settings.mars_enabled;
        self.session_recovery_negotiated = negotiated_settings.is_session_recovery_acknowledged();
        self.session_state_table.seed_initial_state(
            negotiated_settings.database.clone(),
            negotiated_settings.language.clone(),
            negotiated_settings.database_collation,
            login_session_state_tokens,
            negotiated_settings.session_recovery_initial_state(),
        );
    }

    /// Check whether session recovery can be attempted.
    ///
    /// Returns `true` only if all preconditions are met:
    /// - Session recovery was negotiated with the server
    /// - Server has not globally disabled recovery
    /// - No session state entries are marked as non-recoverable
    /// - No batch is currently in progress (results pending consumption)
    /// - No transaction is currently active
    pub fn is_recovery_possible(&self, execution_context: &ExecutionContext) -> bool {
        self.session_recovery_negotiated
            && self.session_state_table.is_session_recoverable()
            && !execution_context.has_open_batch()
            && !execution_context.has_active_transaction()
    }

    /// Validate that a reconnected session matches the original connection properties.
    ///
    /// Checks performed (following ODBC pattern):
    /// - Server acknowledged SessionRecovery feature again
    /// - TDS version matches original
    /// - Server major version matches original
    /// - Encryption level matches original
    /// - MARS setting matches original
    ///
    /// Returns `Ok(())` if all checks pass, or `ReconnectionValidationFailed` on mismatch.
    pub fn validate_reconnection(
        &self,
        new_settings: &crate::handler::handler_factory::NegotiatedSettings,
    ) -> TdsResult<()> {
        use crate::error::Error;

        // Check session recovery was acknowledged
        if !new_settings.is_session_recovery_acknowledged() {
            return Err(Error::ReconnectionValidationFailed(
                "Server did not acknowledge session recovery on reconnection".to_string(),
            ));
        }

        // Check TDS version matches. Option equality naturally handles the
        // uninitialized case: None != Some(_) is a mismatch.
        if self.original_tds_version != new_settings.login_ack_tds_version {
            return Err(Error::ReconnectionValidationFailed(format!(
                "TDS version mismatch: original {:?}, reconnected {:?}",
                self.original_tds_version, new_settings.login_ack_tds_version
            )));
        }

        // Check server major version matches
        let major_versions_match = match (
            self.original_server_version,
            new_settings.login_ack_server_version,
        ) {
            (Some(orig), Some(new_ver)) => orig.major == new_ver.major,
            (None, None) => true,
            _ => false,
        };
        if !major_versions_match {
            return Err(Error::ReconnectionValidationFailed(format!(
                "Server major version mismatch: original {:?}, reconnected {:?}",
                self.original_server_version.map(|v| v.major),
                new_settings.login_ack_server_version.map(|v| v.major)
            )));
        }

        // Check encryption level matches
        if self.original_encryption_level
            != Some(new_settings.session_settings.negotiated_encryption_settings)
        {
            return Err(Error::ReconnectionValidationFailed(format!(
                "Encryption level mismatch: original {:?}, reconnected {:?}",
                self.original_encryption_level,
                new_settings.session_settings.negotiated_encryption_settings
            )));
        }

        // Check MARS setting matches
        if self.original_mars_enabled != new_settings.session_settings.mars_enabled {
            return Err(Error::ReconnectionValidationFailed(format!(
                "MARS setting mismatch: original {}, reconnected {}",
                self.original_mars_enabled, new_settings.session_settings.mars_enabled
            )));
        }

        Ok(())
    }

    /// Process a SESSIONSTATE token received from the server.
    ///
    /// - If `sequence_number == u32::MAX` → server has globally disabled recovery.
    /// - Otherwise, update each state entry in the table.
    pub fn process_session_state(&mut self, token: &SessionStateToken) -> TdsResult<()> {
        if token.sequence_number == u32::MAX {
            self.session_state_table.master_recovery_disabled = true;
            return Ok(());
        }

        for entry in &token.states {
            self.session_state_table.update_state(
                entry.state_id,
                token.sequence_number,
                entry.recoverable,
                entry.data.clone(),
            );
        }
        Ok(())
    }
}

/// A single session state entry tracked by the server.
#[derive(Debug, Clone)]
pub(crate) struct SessionStateRecord {
    pub recoverable: bool,
    #[allow(dead_code)] // Stored for diagnostics; read in tests
    pub sequence: u32,
    pub data: Vec<u8>,
}

/// Tracks session state received from the server for idle connection resiliency.
///
/// The server sends SESSIONSTATE tokens (0xE4) containing state updates during
/// normal operation. On reconnection, the client serializes both the initial
/// state snapshot and the accumulated delta back to the server in the LOGIN7
/// FEATUREEXT for Session Recovery (0x01).
///
/// Layout mirrors JDBC `SessionStateTable` and SqlClient `SessionData`.
#[derive(Debug)]
pub(crate) struct SessionStateTable {
    /// Baseline session state captured from the initial FEATUREEXTACK response.
    pub initial_state: [Option<SessionStateRecord>; 256],
    /// Delta state accumulated from SESSIONSTATE tokens during operation.
    pub delta: [Option<SessionStateRecord>; 256],
    /// Database name at connection time.
    pub initial_database: String,
    /// Language at connection time.
    pub initial_language: String,
    /// Collation at connection time.
    pub initial_collation: SqlCollation,
    /// When true, the server has globally disabled recovery for this session
    /// (signaled by `sequence_number == u32::MAX` in a SESSIONSTATE token).
    pub master_recovery_disabled: bool,
}

// Manual Default because [Option<SessionStateRecord>; 256] doesn't implement
// Default via derive (array size > 32 without const generics Default blanket).
impl Default for SessionStateTable {
    fn default() -> Self {
        Self {
            initial_state: std::array::from_fn(|_| None),
            delta: std::array::from_fn(|_| None),
            initial_database: String::new(),
            initial_language: String::new(),
            initial_collation: SqlCollation::default(),
            master_recovery_disabled: false,
        }
    }
}

/// A parsed FEATUREEXTACK entry: `(state_id, value)`.
type FeatureAckEntry = (u8, Vec<u8>);

/// Where parsing a truncated FEATUREEXTACK payload failed: `(entry_offset, state_id)`.
type FeatureAckParseError = (usize, u8);

impl SessionStateTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update a session state entry from a SESSIONSTATE token.
    ///
    /// Recoverability is evaluated on demand by [`is_session_recoverable`], so
    /// this only stores the latest record for the state id.
    pub fn update_state(&mut self, state_id: u8, sequence: u32, recoverable: bool, data: Vec<u8>) {
        self.delta[state_id as usize] = Some(SessionStateRecord {
            recoverable,
            sequence,
            data,
        });
    }

    /// Seed the baseline snapshot captured at login.
    ///
    /// Called once, right after a successful login, with the database/language/
    /// collation negotiated at connect time and any SESSIONSTATE tokens the
    /// server sent during login. Without this, `initial_*` stays empty and a
    /// reconnect's LOGIN7 session-recovery block goes out blank, which servers
    /// reject.
    pub fn seed_initial_state(
        &mut self,
        database: String,
        language: String,
        collation: SqlCollation,
        tokens: &[SessionStateToken],
        feature_ack_initial_state: Option<&[u8]>,
    ) {
        self.initial_database = database;
        self.initial_language = language;
        self.initial_collation = collation;

        // The FEATUREEXTACK payload is the recoverable baseline (initial state),
        // applied before any SESSIONSTATE token so a same-id token (a
        // post-baseline update) lands in the delta rather than being clobbered.
        // Without it the reconnect LOGIN7 goes out incomplete and the server
        // rejects it as semantically invalid (error 17897, state 81).
        if let Some(data) = feature_ack_initial_state {
            self.seed_initial_state_from_feature_ack(data);
        }

        // SESSIONSTATE tokens observed during login are post-baseline updates,
        // so they belong in the delta — identical to tokens seen mid-session.
        for token in tokens {
            if token.sequence_number == u32::MAX {
                self.master_recovery_disabled = true;
                continue;
            }
            for entry in &token.states {
                self.update_state(
                    entry.state_id,
                    token.sequence_number,
                    entry.recoverable,
                    entry.data.clone(),
                );
            }
        }
    }

    /// Parse the packed FEATUREEXTACK session-recovery payload into
    /// `(state_id, value)` entries. Entry format matches the reconnect wire
    /// format: StateId (1 byte), Length (1 byte, or `0xFF` followed by a u32),
    /// Value. Side-effect-free: returns `Err((entry_offset, state_id))` on a
    /// truncated payload so the caller can disable recovery and log where the
    /// parse ran off the end.
    fn parse_feature_ack(data: &[u8]) -> Result<Vec<FeatureAckEntry>, FeatureAckParseError> {
        let mut entries = Vec::new();
        let mut i = 0;
        while i < data.len() {
            let entry_start = i;
            let state_id = data[i];
            i += 1;

            let Some(&len_byte) = data.get(i) else {
                return Err((entry_start, state_id));
            };
            i += 1;

            let len = if len_byte == 0xFF {
                let Some(bytes) = i.checked_add(4).and_then(|end| data.get(i..end)) else {
                    return Err((entry_start, state_id));
                };
                i += 4;
                u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize
            } else {
                len_byte as usize
            };

            let Some(value) = i.checked_add(len).and_then(|end| data.get(i..end)) else {
                return Err((entry_start, state_id));
            };
            i += len;
            entries.push((state_id, value.to_vec()));
        }
        Ok(entries)
    }

    /// Seed the login baseline (`initial_state`) from the FEATUREEXTACK payload.
    fn seed_initial_state_from_feature_ack(&mut self, data: &[u8]) {
        match Self::parse_feature_ack(data) {
            Ok(entries) => {
                for (state_id, value) in entries {
                    self.initial_state[state_id as usize] = Some(SessionStateRecord {
                        recoverable: true,
                        sequence: 0,
                        data: value,
                    });
                }
            }
            Err((offset, state_id)) => self.abort_partial_baseline(offset, state_id),
        }
    }

    /// Apply a recovery-login FEATUREEXTACK payload to the current (`delta`)
    /// state, mirroring msodbcsql's `ReadUpdatedSessionState(true, 0, ...)`.
    /// Sequence number 0 marks a feature-ack replace, so the current table is
    /// reset and repopulated from the payload rather than merged. The login
    /// baseline is untouched; both blocks serialize on the next reconnect.
    pub(crate) fn apply_feature_ack_to_delta(&mut self, data: &[u8]) {
        self.delta = std::array::from_fn(|_| None);
        match Self::parse_feature_ack(data) {
            Ok(entries) => {
                for (state_id, value) in entries {
                    self.update_state(state_id, 0, true, value);
                }
            }
            Err((offset, state_id)) => self.abort_partial_baseline(offset, state_id),
        }
    }

    /// A truncated or malformed FEATUREEXTACK payload means the baseline cannot
    /// be fully reconstructed. Disable recovery so the next reconnect fails fast
    /// with a driver error instead of sending a partial baseline the server
    /// rejects opaquely (error 17897, state 81). `offset` is the start of the
    /// offending entry, correlating the warning with a wire capture.
    fn abort_partial_baseline(&mut self, offset: usize, state_id: u8) {
        tracing::warn!(
            offset,
            state_id,
            "Malformed session-recovery FEATUREEXTACK payload; disabling recovery"
        );
        self.master_recovery_disabled = true;
    }

    /// Returns `true` if the session can be recovered after a disconnect.
    ///
    /// Recovery is blocked when the server has globally disabled it or when
    /// any state entry is marked non-recoverable (e.g., open cursors, certain
    /// temp tables, specific SET options).
    pub fn is_session_recoverable(&self) -> bool {
        !self.master_recovery_disabled
            && !self
                .initial_state
                .iter()
                .chain(self.delta.iter())
                .flatten()
                .any(|record| !record.recoverable)
    }

    /// Clear accumulated delta state. Called on ENVCHANGE sub-type 18
    /// (connection reset acknowledgment) — all three reference drivers
    /// (ODBC, JDBC, SqlClient) reset state on this event.
    pub fn reset(&mut self) {
        self.delta = std::array::from_fn(|_| None);
        // master_recovery_disabled is NOT cleared — it persists across resets.
        // initial_state is NOT cleared — it represents the baseline snapshot.
    }

    /// Create a frozen snapshot for reconnection serialization.
    ///
    /// `current_database`, `current_language`, and `current_collation` represent
    /// the session's current values (from `EnvChangeProperties`). These are
    /// compared against the initial values to determine what goes in the delta block.
    pub fn snapshot(
        &self,
        current_database: Option<&str>,
        current_language: Option<&str>,
        current_collation: Option<SqlCollation>,
    ) -> Box<SessionRecoveryData> {
        Box::new(SessionRecoveryData {
            initial_state: self.initial_state.clone(),
            delta: self.delta.clone(),
            initial_database: self.initial_database.clone(),
            initial_language: self.initial_language.clone(),
            initial_collation: self.initial_collation,
            database: current_database
                .unwrap_or(&self.initial_database)
                .to_string(),
            language: current_language
                .unwrap_or(&self.initial_language)
                .to_string(),
            collation: current_collation.unwrap_or(self.initial_collation),
        })
    }
}

/// A frozen snapshot of session state used during reconnection serialization.
///
/// Created via [`SessionStateTable::snapshot()`] before initiating reconnection.
/// Contains cloned copies so the original table can be safely dropped or mutated
/// during the reconnect attempt.
#[derive(Debug, Clone)]
pub(crate) struct SessionRecoveryData {
    pub initial_state: [Option<SessionStateRecord>; 256],
    pub delta: [Option<SessionStateRecord>; 256],
    pub initial_database: String,
    pub initial_language: String,
    pub initial_collation: SqlCollation,
    /// Current database (may differ from initial after USE [db]).
    pub database: String,
    /// Current language (may differ from initial after SET LANGUAGE).
    pub language: String,
    /// Current collation (may differ from initial).
    pub collation: SqlCollation,
}

impl SessionRecoveryData {
    /// Compute the byte length of the initial state block (excluding the DWORD length prefix).
    pub fn initial_block_length(&self) -> u32 {
        let mut len: u32 = 0;
        // B_VARCHAR: initial_database
        len += 1 + 2 * self.initial_database.encode_utf16().count() as u32;
        // Collation: 1 byte length + 5 bytes data (or just 1 byte if default/zero)
        len += if self.initial_collation == SqlCollation::default() {
            1 // length byte = 0
        } else {
            6 // length byte (5) + 4 bytes info + 1 byte sort_id
        };
        // B_VARCHAR: initial_language
        len += 1 + 2 * self.initial_language.encode_utf16().count() as u32;
        // State entries
        for entry in &self.initial_state {
            if let Some(record) = entry.as_ref() {
                len += 1; // state_id
                len += state_value_wire_length(record.data.len());
            }
        }
        len
    }

    /// Compute the byte length of the delta block (excluding the DWORD length prefix).
    pub fn delta_block_length(&self) -> u32 {
        let mut len: u32 = 0;
        // Database: 0 if same, else B_VARCHAR
        if self.database == self.initial_database {
            len += 1;
        } else {
            len += 1 + 2 * self.database.encode_utf16().count() as u32;
        }
        // Collation: 0 if same, else 6
        if self.collation == self.initial_collation {
            len += 1;
        } else {
            len += 6;
        }
        // Language: 0 if same, else B_VARCHAR
        if self.language == self.initial_language {
            len += 1;
        } else {
            len += 1 + 2 * self.language.encode_utf16().count() as u32;
        }
        // Delta state entries
        for entry in &self.delta {
            if let Some(record) = entry.as_ref() {
                len += 1; // state_id
                len += state_value_wire_length(record.data.len());
            }
        }
        len
    }

    /// Total data length field value: 8 (two DWORD sub-block lengths) + initial + delta.
    pub fn total_data_length(&self) -> u32 {
        8 + self.initial_block_length() + self.delta_block_length()
    }
}

/// Compute the on-wire byte count for a state entry's length prefix + data.
///
/// If `data_len < 0xFF`: 1-byte length prefix + data bytes.
/// If `data_len >= 0xFF`: 1-byte marker (0xFF) + 4-byte DWORD length + data bytes.
fn state_value_wire_length(data_len: usize) -> u32 {
    if data_len < 0xFF {
        1 + data_len as u32
    } else {
        5 + data_len as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_table_is_recoverable() {
        let table = SessionStateTable::new();
        assert!(table.is_session_recoverable());
        assert!(!table.master_recovery_disabled);
    }

    #[test]
    fn update_recoverable_state_keeps_session_recoverable() {
        let mut table = SessionStateTable::new();
        table.update_state(0, 1, true, vec![0x01, 0x02]);

        assert!(table.is_session_recoverable());
        assert!(table.delta[0].is_some());
        let record = table.delta[0].as_ref().unwrap();
        assert!(record.recoverable);
        assert_eq!(record.sequence, 1);
        assert_eq!(record.data, vec![0x01, 0x02]);
    }

    #[test]
    fn update_unrecoverable_state_blocks_recovery() {
        let mut table = SessionStateTable::new();
        table.update_state(5, 1, false, vec![0xAA]);

        assert!(!table.is_session_recoverable());
    }

    #[test]
    fn transition_unrecoverable_to_recoverable_restores_recovery() {
        let mut table = SessionStateTable::new();
        // First update: unrecoverable
        table.update_state(5, 1, false, vec![0xAA]);
        assert!(!table.is_session_recoverable());

        // Second update: same state_id becomes recoverable
        table.update_state(5, 2, true, vec![0xBB]);
        assert!(table.is_session_recoverable());

        let record = table.delta[5].as_ref().unwrap();
        assert!(record.recoverable);
        assert_eq!(record.sequence, 2);
        assert_eq!(record.data, vec![0xBB]);
    }

    #[test]
    fn transition_recoverable_to_unrecoverable_blocks_recovery() {
        let mut table = SessionStateTable::new();
        table.update_state(10, 1, true, vec![0x01]);
        assert!(table.is_session_recoverable());

        table.update_state(10, 2, false, vec![0x02]);
        assert!(!table.is_session_recoverable());
    }

    #[test]
    fn multiple_unrecoverable_states_require_all_cleared() {
        let mut table = SessionStateTable::new();
        table.update_state(1, 1, false, vec![0x01]);
        table.update_state(2, 1, false, vec![0x02]);
        assert!(!table.is_session_recoverable());

        // Clear one — still blocked
        table.update_state(1, 2, true, vec![0x03]);
        assert!(!table.is_session_recoverable());

        // Clear the other — now recoverable
        table.update_state(2, 2, true, vec![0x04]);
        assert!(table.is_session_recoverable());
    }

    #[test]
    fn repeated_update_same_recoverability_no_count_change() {
        let mut table = SessionStateTable::new();
        table.update_state(0, 1, false, vec![0x01]);
        assert!(!table.is_session_recoverable());

        // Update again with same recoverability — count should not double
        table.update_state(0, 2, false, vec![0x02]);
        assert!(!table.is_session_recoverable());

        // Single transition to recoverable should clear it
        table.update_state(0, 3, true, vec![0x03]);
        assert!(table.is_session_recoverable());
    }

    #[test]
    fn master_recovery_disabled_blocks_even_with_no_unrecoverable_states() {
        let mut table = SessionStateTable::new();
        table.master_recovery_disabled = true;

        assert!(!table.is_session_recoverable());
    }

    #[test]
    fn reset_clears_delta_and_unrecoverable_count() {
        let mut table = SessionStateTable::new();
        table.update_state(0, 1, false, vec![0x01]);
        table.update_state(5, 1, true, vec![0x02]);
        assert!(!table.is_session_recoverable());

        table.reset();

        assert!(table.is_session_recoverable());
        assert!(table.delta[0].is_none());
        assert!(table.delta[5].is_none());
    }

    #[test]
    fn seed_from_feature_ack_parses_server_baseline() {
        // The 8-entry baseline the server sent in the SESSIONRECOVERY
        // FEATUREEXTACK at login (captured from the wire). Dropping any of
        // these makes the reconnect LOGIN7 fail with error 17897, state 81.
        let ack = vec![
            0x00, 0x09, 0x00, 0x60, 0x81, 0x14, 0xFF, 0xE7, 0xFF, 0xFF, 0x00, // id 0, len 9
            0x02, 0x02, 0x07, 0x01, // id 2, len 2
            0x04, 0x01, 0x00, // id 4, len 1
            0x05, 0x04, 0xFF, 0xFF, 0xFF, 0xFF, // id 5, len 4
            0x06, 0x01, 0x00, // id 6, len 1
            0x07, 0x01, 0x02, // id 7, len 1
            0x08, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // id 8, len 8
            0x09, 0x04, 0xFF, 0xFF, 0xFF, 0xFF, // id 9, len 4
        ];
        let mut table = SessionStateTable::new();
        table.seed_initial_state_from_feature_ack(&ack);

        for (id, expected) in [
            (
                0usize,
                vec![0x00, 0x60, 0x81, 0x14, 0xFF, 0xE7, 0xFF, 0xFF, 0x00],
            ),
            (2, vec![0x07, 0x01]),
            (4, vec![0x00]),
            (5, vec![0xFF, 0xFF, 0xFF, 0xFF]),
            (6, vec![0x00]),
            (7, vec![0x02]),
            (8, vec![0x00; 8]),
            (9, vec![0xFF, 0xFF, 0xFF, 0xFF]),
        ] {
            let record = table.initial_state[id]
                .as_ref()
                .unwrap_or_else(|| panic!("state {id} missing"));
            assert_eq!(record.data, expected);
            assert!(record.recoverable);
            assert_eq!(record.sequence, 0);
        }
        // Gaps stay empty and the session remains recoverable.
        assert!(table.initial_state[1].is_none());
        assert!(table.initial_state[3].is_none());
        assert!(table.is_session_recoverable());
    }

    #[test]
    fn seed_from_feature_ack_extended_length() {
        // Values >= 0xFF use the 0xFF marker followed by a u32 length.
        let mut ack = vec![10u8, 0xFF];
        ack.extend_from_slice(&300u32.to_le_bytes());
        ack.extend_from_slice(&[0xAB; 300]);

        let mut table = SessionStateTable::new();
        table.seed_initial_state_from_feature_ack(&ack);

        let record = table.initial_state[10].as_ref().unwrap();
        assert_eq!(record.data.len(), 300);
        assert!(record.data.iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn seed_from_feature_ack_truncated_disables_recovery() {
        // A complete entry followed by one whose declared length runs past the
        // buffer: the parse aborts all-or-nothing (no entries applied) and
        // recovery is disabled so the next reconnect fails fast rather than
        // sending a partial baseline.
        let ack = vec![3, 1, 0xAA, 5, 10, 0x01];
        let mut table = SessionStateTable::new();
        table.seed_initial_state_from_feature_ack(&ack);

        assert!(table.initial_state[3].is_none());
        assert!(table.initial_state[5].is_none());
        assert!(table.master_recovery_disabled);
        assert!(!table.is_session_recoverable());
    }

    #[test]
    fn apply_feature_ack_to_delta_resets_then_reloads() {
        // seq 0 (feature ack) replaces the current table: a stale delta entry is
        // cleared and only the payload's entries remain; the login baseline is
        // untouched (msodbcsql ReadUpdatedSessionState parity).
        let ack = vec![7, 2, 0x01, 0x02];
        let mut table = SessionStateTable::new();
        table.update_state(3, 5, true, vec![0xAA]);
        table.apply_feature_ack_to_delta(&ack);

        assert!(table.delta[3].is_none());
        assert!(table.initial_state[7].is_none());
        let record = table.delta[7].as_ref().unwrap();
        assert_eq!(record.data, vec![0x01, 0x02]);
        assert!(record.recoverable);
        assert_eq!(record.sequence, 0);
    }

    #[test]
    fn seed_initial_state_forwards_feature_ack() {
        let ack = vec![7, 2, 0x01, 0x02];
        let mut table = SessionStateTable::new();
        table.seed_initial_state(
            "master".to_string(),
            "us_english".to_string(),
            SqlCollation::default(),
            &[],
            Some(&ack),
        );

        assert_eq!(table.initial_database, "master");
        assert_eq!(
            table.initial_state[7].as_ref().unwrap().data,
            vec![0x01, 0x02]
        );
    }

    #[test]
    fn seed_initial_state_from_tokens_tracks_recoverability() {
        use crate::token::tokens::SessionStateEntry;

        // A non-recoverable login-time SESSIONSTATE entry blocks recovery.
        let mut table = SessionStateTable::new();
        table.seed_initial_state(
            "master".to_string(),
            "us_english".to_string(),
            SqlCollation::default(),
            &[SessionStateToken {
                sequence_number: 1,
                status: 0,
                states: vec![SessionStateEntry {
                    state_id: 2,
                    recoverable: false,
                    data: vec![0x01],
                }],
            }],
            None,
        );
        assert!(!table.is_session_recoverable());

        // sequence_number == u32::MAX globally disables recovery.
        let mut table = SessionStateTable::new();
        table.seed_initial_state(
            String::new(),
            String::new(),
            SqlCollation::default(),
            &[SessionStateToken {
                sequence_number: u32::MAX,
                status: 0,
                states: vec![],
            }],
            None,
        );
        assert!(table.master_recovery_disabled);
        assert!(!table.is_session_recoverable());
    }

    #[test]
    fn reset_preserves_master_recovery_disabled() {
        let mut table = SessionStateTable::new();
        table.master_recovery_disabled = true;
        table.reset();

        assert!(table.master_recovery_disabled);
        assert!(!table.is_session_recoverable());
    }

    #[test]
    fn reset_preserves_initial_state() {
        let mut table = SessionStateTable::new();
        table.initial_state[0] = Some(SessionStateRecord {
            recoverable: true,
            sequence: 1,
            data: vec![0x01],
        });
        table.initial_database = "testdb".to_string();

        table.reset();

        assert!(table.initial_state[0].is_some());
        assert_eq!(table.initial_database, "testdb");
    }

    #[test]
    fn snapshot_creates_independent_copy() {
        let mut table = SessionStateTable::new();
        table.initial_database = "mydb".to_string();
        table.initial_language = "us_english".to_string();
        table.update_state(0, 1, true, vec![0x01]);

        let snapshot = table.snapshot(None, None, None);

        // Snapshot has the data
        assert_eq!(snapshot.initial_database, "mydb");
        assert_eq!(snapshot.initial_language, "us_english");
        // Current values default to initial when None is passed
        assert_eq!(snapshot.database, "mydb");
        assert_eq!(snapshot.language, "us_english");
        assert!(snapshot.delta[0].is_some());

        // Mutating original doesn't affect snapshot
        table.initial_database = "other".to_string();
        table.update_state(0, 2, false, vec![0x02]);

        assert_eq!(snapshot.initial_database, "mydb");
        assert!(snapshot.delta[0].as_ref().unwrap().recoverable);
    }

    #[test]
    fn all_256_state_ids_can_be_used() {
        let mut table = SessionStateTable::new();
        for i in 0..=255u8 {
            table.update_state(i, 1, true, vec![i]);
        }
        assert!(table.is_session_recoverable());
        assert!(table.delta[0].is_some());
        assert!(table.delta[255].is_some());
    }

    // ── RecoveryContext tests ──

    use crate::token::tokens::{SessionStateEntry, SessionStateToken};

    fn make_session_state_token(
        sequence_number: u32,
        states: Vec<SessionStateEntry>,
    ) -> SessionStateToken {
        SessionStateToken {
            sequence_number,
            status: 0,
            states,
        }
    }

    fn make_entry(state_id: u8, recoverable: bool, data: Vec<u8>) -> SessionStateEntry {
        SessionStateEntry {
            state_id,
            recoverable,
            data,
        }
    }

    #[test]
    fn recovery_context_new_defaults() {
        let ctx = RecoveryContext::new();
        assert!(!ctx.session_recovery_negotiated);
        assert!(ctx.session_state_table.is_session_recoverable());
    }

    #[test]
    fn process_session_state_updates_table() {
        let mut ctx = RecoveryContext::new();
        let token = make_session_state_token(
            1,
            vec![
                make_entry(0, true, vec![0x01]),
                make_entry(5, false, vec![0x02]),
            ],
        );
        ctx.process_session_state(&token).unwrap();

        assert!(ctx.session_state_table.delta[0].is_some());
        assert!(ctx.session_state_table.delta[5].is_some());
        assert!(!ctx.session_state_table.is_session_recoverable()); // state 5 is unrecoverable
    }

    #[test]
    fn process_session_state_master_disable() {
        let mut ctx = RecoveryContext::new();
        let token = make_session_state_token(u32::MAX, vec![make_entry(0, true, vec![0x01])]);
        ctx.process_session_state(&token).unwrap();

        assert!(ctx.session_state_table.master_recovery_disabled);
        // State entries should NOT be processed when master disable is signaled
        assert!(ctx.session_state_table.delta[0].is_none());
    }

    #[test]
    fn process_session_state_multiple_tokens_accumulate() {
        let mut ctx = RecoveryContext::new();

        let token1 = make_session_state_token(1, vec![make_entry(0, true, vec![0x01])]);
        ctx.process_session_state(&token1).unwrap();

        let token2 = make_session_state_token(2, vec![make_entry(1, true, vec![0x02])]);
        ctx.process_session_state(&token2).unwrap();

        assert!(ctx.session_state_table.delta[0].is_some());
        assert!(ctx.session_state_table.delta[1].is_some());
        assert_eq!(
            ctx.session_state_table.delta[0].as_ref().unwrap().sequence,
            1
        );
        assert_eq!(
            ctx.session_state_table.delta[1].as_ref().unwrap().sequence,
            2
        );
    }

    #[test]
    fn process_session_state_overwrites_same_state_id() {
        let mut ctx = RecoveryContext::new();

        let token1 = make_session_state_token(1, vec![make_entry(0, true, vec![0x01])]);
        ctx.process_session_state(&token1).unwrap();

        let token2 = make_session_state_token(2, vec![make_entry(0, true, vec![0xFF])]);
        ctx.process_session_state(&token2).unwrap();

        let record = ctx.session_state_table.delta[0].as_ref().unwrap();
        assert_eq!(record.sequence, 2);
        assert_eq!(record.data, vec![0xFF]);
    }

    // ── Recovery eligibility tests ──

    fn make_initialized_context() -> RecoveryContext {
        use crate::message::features::session_recovery::SessionRecoveryFeature;
        use crate::message::login::Feature;

        let mut ctx = RecoveryContext::new();
        let client_ctx = crate::connection::client_context::ClientContext::with_data_source(
            "tcp:localhost,1433",
        );

        let mut settings =
            crate::handler::handler_factory::create_test_negotiated_settings_internal();
        settings.login_ack_tds_version = Some(TdsVersion::V7_4);
        settings.login_ack_server_version = Some(Version::new(16, 0, 1000, 0));
        settings.session_settings.negotiated_encryption_settings =
            NegotiatedEncryptionSetting::Mandatory;
        let mut feature = SessionRecoveryFeature::new(1);
        feature.set_acknowledged(true);
        settings
            .session_settings
            .supported_features
            .push(Box::new(feature));

        ctx.initialize(client_ctx, &settings, &[]);
        ctx
    }

    #[test]
    fn initialize_captures_all_fields() {
        let ctx = make_initialized_context();
        assert!(ctx.client_context.is_some());
        assert_eq!(ctx.original_tds_version, Some(TdsVersion::V7_4));
        assert_eq!(
            ctx.original_server_version,
            Some(Version::new(16, 0, 1000, 0))
        );
        assert_eq!(
            ctx.original_encryption_level,
            Some(NegotiatedEncryptionSetting::Mandatory)
        );
        assert!(!ctx.original_mars_enabled);
        assert_eq!(ctx.recovery_count, 0);
    }

    #[test]
    fn is_recovery_possible_all_conditions_met() {
        let ctx = make_initialized_context();
        let exec = crate::connection::execution_context::ExecutionContext::new();
        assert!(ctx.is_recovery_possible(&exec));
    }

    #[test]
    fn is_recovery_possible_false_when_not_negotiated() {
        let mut ctx = make_initialized_context();
        ctx.session_recovery_negotiated = false;
        let exec = crate::connection::execution_context::ExecutionContext::new();
        assert!(!ctx.is_recovery_possible(&exec));
    }

    #[test]
    fn is_recovery_possible_false_when_master_disabled() {
        let mut ctx = make_initialized_context();
        ctx.session_state_table.master_recovery_disabled = true;
        let exec = crate::connection::execution_context::ExecutionContext::new();
        assert!(!ctx.is_recovery_possible(&exec));
    }

    #[test]
    fn is_recovery_possible_false_when_unrecoverable_state() {
        let mut ctx = make_initialized_context();
        ctx.session_state_table
            .update_state(5, 1, false, vec![0x01]);
        let exec = crate::connection::execution_context::ExecutionContext::new();
        assert!(!ctx.is_recovery_possible(&exec));
    }

    #[test]
    fn is_recovery_possible_false_when_batch_open() {
        let ctx = make_initialized_context();
        let mut exec = crate::connection::execution_context::ExecutionContext::new();
        exec.set_has_open_batch(true);
        assert!(!ctx.is_recovery_possible(&exec));
    }

    #[test]
    fn is_recovery_possible_false_when_transaction_active() {
        let ctx = make_initialized_context();
        let mut exec = crate::connection::execution_context::ExecutionContext::new();
        exec.set_transaction_descriptor(12345);
        assert!(!ctx.is_recovery_possible(&exec));
    }

    #[test]
    fn is_recovery_possible_false_multiple_blockers() {
        let mut ctx = make_initialized_context();
        ctx.session_state_table.master_recovery_disabled = true;
        let mut exec = crate::connection::execution_context::ExecutionContext::new();
        exec.set_has_open_batch(true);
        exec.set_transaction_descriptor(1);
        assert!(!ctx.is_recovery_possible(&exec));
    }

    #[test]
    fn is_recovery_possible_recovers_after_clearing_state() {
        let mut ctx = make_initialized_context();
        ctx.session_state_table
            .update_state(5, 1, false, vec![0x01]);
        let exec = crate::connection::execution_context::ExecutionContext::new();
        assert!(!ctx.is_recovery_possible(&exec));

        // Overwrite the non-recoverable state with a recoverable one
        ctx.session_state_table.update_state(5, 2, true, vec![0x02]);
        assert!(ctx.is_recovery_possible(&exec));
    }

    #[test]
    fn debug_format_hides_client_context() {
        let ctx = make_initialized_context();
        let debug_str = format!("{:?}", ctx);
        assert!(debug_str.contains("<ClientContext>"));
        assert!(!debug_str.contains("password"));
    }

    // Reconnection validation tests

    use crate::handler::handler_factory::{
        NegotiatedSettings, create_test_negotiated_settings_internal,
    };
    use crate::message::features::session_recovery::SessionRecoveryFeature;
    use crate::message::login::Feature;

    /// Create NegotiatedSettings that match the values set by make_initialized_context().
    fn make_matching_negotiated_settings() -> NegotiatedSettings {
        let mut settings = create_test_negotiated_settings_internal();
        // Match the values from make_initialized_context()
        settings.login_ack_tds_version = Some(TdsVersion::V7_4);
        settings.login_ack_server_version = Some(Version::new(16, 0, 1000, 0));
        settings.session_settings.negotiated_encryption_settings =
            NegotiatedEncryptionSetting::Mandatory;
        settings.session_settings.mars_enabled = false;
        // Add an acknowledged session recovery feature
        let mut feature = SessionRecoveryFeature::new(1);
        feature.set_acknowledged(true);
        settings
            .session_settings
            .supported_features
            .push(Box::new(feature));
        settings
    }

    #[test]
    fn validate_reconnection_all_matching() {
        let ctx = make_initialized_context();
        let settings = make_matching_negotiated_settings();
        assert!(ctx.validate_reconnection(&settings).is_ok());
    }

    #[test]
    fn validate_reconnection_fails_when_session_recovery_not_acknowledged() {
        let ctx = make_initialized_context();
        let mut settings = make_matching_negotiated_settings();
        // Remove the session recovery feature
        settings.session_settings.supported_features.clear();
        let err = ctx.validate_reconnection(&settings).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("did not acknowledge session recovery"));
    }

    #[test]
    fn validate_reconnection_fails_when_feature_present_but_not_acknowledged() {
        let ctx = make_initialized_context();
        let mut settings = make_matching_negotiated_settings();
        // Replace with unacknowledged feature
        settings.session_settings.supported_features.clear();
        let feature = SessionRecoveryFeature::new(1); // not acknowledged
        settings
            .session_settings
            .supported_features
            .push(Box::new(feature));
        let err = ctx.validate_reconnection(&settings).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("did not acknowledge session recovery"));
    }

    #[test]
    fn validate_reconnection_fails_on_tds_version_mismatch() {
        let ctx = make_initialized_context();
        let mut settings = make_matching_negotiated_settings();
        settings.login_ack_tds_version = Some(TdsVersion::V8_0);
        let err = ctx.validate_reconnection(&settings).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("TDS version mismatch"));
    }

    #[test]
    fn validate_reconnection_fails_on_server_major_version_mismatch() {
        let ctx = make_initialized_context();
        let mut settings = make_matching_negotiated_settings();
        settings.login_ack_server_version = Some(Version::new(15, 0, 2000, 0));
        let err = ctx.validate_reconnection(&settings).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("Server major version mismatch"));
    }

    #[test]
    fn validate_reconnection_ok_when_server_minor_version_differs() {
        let ctx = make_initialized_context();
        let mut settings = make_matching_negotiated_settings();
        // Same major (16), different minor
        settings.login_ack_server_version = Some(Version::new(16, 5, 9999, 0));
        assert!(ctx.validate_reconnection(&settings).is_ok());
    }

    #[test]
    fn validate_reconnection_fails_on_encryption_mismatch() {
        let ctx = make_initialized_context();
        let mut settings = make_matching_negotiated_settings();
        settings.session_settings.negotiated_encryption_settings =
            NegotiatedEncryptionSetting::NoEncryption;
        let err = ctx.validate_reconnection(&settings).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("Encryption level mismatch"));
    }

    #[test]
    fn validate_reconnection_fails_on_mars_mismatch() {
        let ctx = make_initialized_context();
        let mut settings = make_matching_negotiated_settings();
        settings.session_settings.mars_enabled = true;
        let err = ctx.validate_reconnection(&settings).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("MARS setting mismatch"));
    }

    #[test]
    fn validate_reconnection_fails_when_original_tds_version_is_none() {
        let mut ctx = make_initialized_context();
        ctx.original_tds_version = None;
        let settings = make_matching_negotiated_settings();
        let err = ctx.validate_reconnection(&settings).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("TDS version mismatch"));
    }

    #[test]
    fn validate_reconnection_fails_when_original_server_version_is_none() {
        let mut ctx = make_initialized_context();
        ctx.original_server_version = None;
        let settings = make_matching_negotiated_settings();
        let err = ctx.validate_reconnection(&settings).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("Server major version mismatch"));
    }

    #[test]
    fn validate_reconnection_fails_when_original_encryption_is_none() {
        let mut ctx = make_initialized_context();
        ctx.original_encryption_level = None;
        let settings = make_matching_negotiated_settings();
        let err = ctx.validate_reconnection(&settings).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("Encryption level mismatch"));
    }
}
