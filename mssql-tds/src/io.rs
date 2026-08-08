// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! # I/O Module
//!
//! Low-level packet reading, writing, and token stream processing for the TDS protocol.
//!
//! This module provides the foundation for all network communication in the mssql-tds library.
//! It handles:
//!
//! - **Packet I/O**: Reading and writing TDS packets ([`packet_reader`], [`packet_writer`])
//! - **Token Streaming**: Parsing token streams from server responses
//! - **Network Abstractions**: Traits for network read/write operations ([`reader_writer`])
//!
//! ## Module Organization
//!
//! - [`packet_reader`] - Deserializes TDS packets from the network stream
//! - [`packet_writer`] - Serializes data into TDS packets for transmission
//! - [`reader_writer`] - Traits and types for network I/O operations
//! - `token_stream` - Parses token streams returned by SQL Server (internal)
//!
//! ## Usage
//!
//! Most users will not interact with this module directly. Instead, use the higher-level
//! [`crate::connection`] APIs which handle packet I/O internally.

pub(crate) mod packet_buffer;
pub mod packet_reader;
pub mod packet_writer;
pub mod reader_writer;
pub(crate) mod token_stream;
