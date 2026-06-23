# Connection Reset (RESETCONNECTION / RESETCONNECTIONSKIPTRAN) Support in mssql-tds

## Goal

Add client-side sending of the TDS packet-header status bits `0x08`
(RESETCONNECTION) and `0x10` (RESETCONNECTIONSKIPTRAN) on the first packet of
the next Batch / RPC / Transaction-Manager request, so that a connection-pooling
layer can reset a reused connection back to its login defaults (the protocol
equivalent of `sp_reset_connection`).

The two status bits already exist in `PacketStatusFlags` but are marked
`#[allow(dead_code)]` and are never sent.

## Background / Research

### MS-TDS specification (§2.2.3.1.2, packet header Status byte)

- `0x08` **RESETCONNECTION** — client → server. Reset the connection to login
  defaults *before* processing the request (simulates logout + login). Valid
  only on SQL Batch, RPC, and Transaction-Manager requests. MUST be set on the
  **first packet** of the message. MUST NOT be combined with `0x10`.
- `0x10` **RESETCONNECTIONSKIPTRAN** — same as `0x08`, but does **not** modify
  the transaction state (a local or enlisted/distributed transaction survives
  the reset). MUST NOT be combined with `0x08`.
- The server acknowledges via an `ENVCHANGE` (subtype 18, `ResetConnection`) /
  `SESSIONSTATE` reset. `tds_client.rs` reacts to the token by resetting the
  recovery/session-state table, and `execution_context::capture_change_property`
  treats the `ResetConnection` subtype as an informational no-op. (Originally
  this subtype returned an `UnimplementedFeature` error, which caused the first
  query after a reset to fail; this was found and fixed via the integration
  tests running against a live server.)

### How the other drivers implement it

- **dotnet-sqlclient — uses SKIPTRAN.** `ST_RESET_CONNECTION = 0x08` and
  `ST_RESET_CONNECTION_PRESERVE_TRANSACTION = 0x10` (`TdsEnums.cs`).
  `SqlConnectionInternal.ResetConnection()` calls
  `PrepareResetConnection(EnlistedTransaction != null && Pool != null)`, and
  `CheckResetConnection()` ORs the chosen bit into the status byte of the first
  outgoing packet (`_outBuff[1]`). For non-MARS connections the flag is cleared
  immediately after sending.
- **msodbcsql — does not use SKIPTRAN.** Only `TDS_ST_CONNECTION_RESET = 0x08`;
  set on the first packet for SQL / RPC / TRANS message types.

Because dotnet-sqlclient uses SKIPTRAN, we implement **both** `0x08` and `0x10`.

## Design Decisions

- **Explicit parameter API** (mirrors dotnet): the caller/pool decides whether to
  preserve the transaction. The TDS library stays policy-free.
- **Entry point**: a public method on `TdsClient`, intended to be called by an
  external pooling layer when a connection is checked out.
- **One-shot semantics**: the reset flag applies to the next request only and is
  cleared after the first packet is sent. (MARS resend-until-ack is out of scope;
  MARS is not fully implemented in the crate.)

## Rules to Enforce

- The reset bit is set only on the **first packet** of a message.
- Only for `SqlBatch` (0x01), `RpcRequest` (0x03), and `TransactionManager`
  (0x0E) packet types.
- `0x08` and `0x10` are mutually exclusive.
- Combined with `Eom` on single-packet messages (e.g. `0x09 = EOM | RESET`).

## Implementation Steps

Rather than threading a `reset_mode` argument through the ~25
`create_packet_writer` call sites, the pending reset is stored as
connection-level state on the transport (which already owns connection-scoped
state such as encryption and session settings) and consumed once when the first
packet of an eligible request is built.

1. **`src/message/messages.rs`** — add a `ResetConnectionMode { None, Reset,
   ResetSkipTran }` enum; remove the `#[allow(dead_code)]` markers on the two
   `PacketStatusFlags` reset variants once they are used.
2. **`src/io/reader_writer.rs`** — extend the `NetworkWriter` trait with default
   no-op `set_reset_mode(&mut self, ResetConnectionMode)` and
   `take_reset_mode(&mut self) -> ResetConnectionMode` methods, so existing test
   mocks need no changes.
3. **`src/connection/transport/network_transport.rs`** — add a `pending_reset:
   ResetConnectionMode` field (init `None`) to `NetworkTransport` and override
   the two trait methods; `take_reset_mode` returns and clears the field
   (`std::mem::replace`).
4. **`src/io/packet_writer.rs`** — in `PacketWriter::new`, consume the pending
   reset via `take_reset_mode` only for `SqlBatch` / `RpcRequest` /
   `TransactionManager` packet types; store it on `PacketWriter`. In
   `populate_header_and_send`, pass the mode to `build_header` only when
   `self.is_first_packet` is true, where it is OR'd into the status byte.
5. **`src/connection/tds_client.rs`** — add a public
   `prepare_reset_connection(&mut self, preserve_transaction: bool)` method that
   maps the flag to `ResetConnectionMode::ResetSkipTran` / `Reset` and stores it
   on the transport via `set_reset_mode`. One-shot semantics are handled by the
   transport clearing the field when `PacketWriter::new` consumes it.
6. **Tests** — `packet_writer` tests verifying the first packet of single- and
   multi-packet messages carries `EOM | 0x08` / `EOM | 0x10` while later packets
   do not, and that the mode is not consumed for non-request packet types; a
   `tds_client` test confirming the mode is routed to the transport.

## Verification

- `cargo bfmt`, `cargo bclippy`, and `cargo btest` (the repo's aliases from
  `.cargo/config.toml` for `fmt --check`, workspace clippy with warnings denied,
  and the nextest + coverage test run) — the repo's PR validation gates on all
  three.
- Unit tests assert the header status byte on the first vs. subsequent packets.
- Integration test: set a session option / create a temp table,
  `prepare_reset_connection(false)`, re-execute and confirm the state is gone;
  with `preserve_transaction = true` inside a transaction, confirm the
  transaction survives the reset.
