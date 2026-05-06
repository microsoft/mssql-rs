// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mock TDS Server CLI
//!
//! A command-line tool to run a mock TDS server for testing SQL Server clients.
//!
//! # Examples
//!
//! Start a server without TLS on port 1433:
//! ```bash
//! mssql-mock-tds --port 1433
//! ```
//!
//! Start a server with TLS (TDS 7.4 mode) on port 1433:
//! ```bash
//! mssql-mock-tds --port 1433 --tls-mode optional --cert path/to/cert.pem --key path/to/key.pem
//! ```
//!
//! Start a server with strict TLS (TDS 8.0 mode):
//! ```bash
//! mssql-mock-tds --port 1433 --tls-mode strict --cert path/to/cert.pem --key path/to/key.pem
//! ```

use clap::{Parser, ValueEnum};
use mssql_mock_tds::MockTdsServer;
#[cfg(not(windows))]
use std::fs;
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, ValueEnum)]
enum TlsMode {
    /// No TLS (unencrypted connection)
    None,
    /// TDS 7.4 style TLS (wrapped TLS handshake after PreLogin)
    Optional,
    /// TDS 8.0 style strict TLS (TLS handshake immediately on connection)
    Strict,
}

#[derive(Parser, Debug)]
#[command(
    name = "mssql-mock-tds",
    about = "Mock TDS Server for testing SQL Server clients",
    long_about = "A mock TDS (Tabular Data Stream) server that implements enough of the TDS protocol \
                  to test SQL Server client connectivity. Supports both TDS 7.4 (optional TLS) and \
                  TDS 8.0 (strict TLS) modes."
)]
struct Args {
    /// Host address to bind to
    #[arg(short = 'H', long, default_value = "127.0.0.1")]
    host: String,

    /// Port to listen on
    #[arg(short, long, default_value = "1433")]
    port: u16,

    /// TLS mode: none, optional (TDS 7.4), or strict (TDS 8.0)
    #[arg(short = 'm', long, value_enum, default_value = "none")]
    tls_mode: TlsMode,

    /// Path to PEM certificate file (required for TLS modes)
    #[arg(short, long)]
    cert: Option<String>,

    /// Path to PEM private key file (required for TLS modes)
    #[arg(short, long)]
    key: Option<String>,

    /// Path to PKCS#12 (.pfx) identity file (alternative to cert+key)
    #[arg(long)]
    pfx: Option<String>,

    /// Password for PKCS#12 file (if encrypted)
    #[arg(long, default_value = "")]
    pfx_password: String,

    /// Enable connection redirection: host to redirect clients to
    #[arg(long)]
    redirect_host: Option<String>,

    /// Enable connection redirection: port to redirect clients to (requires --redirect-host)
    #[arg(long, default_value = "1433")]
    redirect_port: u16,

    /// Enable verbose logging (can be repeated for more verbosity)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn init_tracing(verbose: u8) {
    let filter = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .init();
}

#[cfg(not(windows))]
fn load_identity_from_pem(
    cert_path: &str,
    key_path: &str,
) -> Result<native_tls::Identity, Box<dyn std::error::Error>> {
    let cert_pem = fs::read(cert_path)?;
    let key_pem = fs::read(key_path)?;
    mssql_mock_tds::create_test_identity(&cert_pem, &key_pem)
}

#[cfg(windows)]
fn load_identity_from_pem(
    _cert_path: &str,
    _key_path: &str,
) -> Result<native_tls::Identity, Box<dyn std::error::Error>> {
    Err("Loading PEM certificates is not supported on Windows. Use --pfx instead.".into())
}

fn load_identity(args: &Args) -> Result<native_tls::Identity, Box<dyn std::error::Error>> {
    // First try PKCS#12 (.pfx) file
    if let Some(pfx_path) = &args.pfx {
        info!("Loading identity from PKCS#12 file: {}", pfx_path);
        return mssql_mock_tds::load_identity_from_file(pfx_path, &args.pfx_password);
    }

    // Then try PEM files
    match (&args.cert, &args.key) {
        (Some(cert_path), Some(key_path)) => {
            info!(
                "Loading identity from PEM files: cert={}, key={}",
                cert_path, key_path
            );
            load_identity_from_pem(cert_path, key_path)
        }
        (Some(_), None) => Err("--cert requires --key to also be specified".into()),
        (None, Some(_)) => Err("--key requires --cert to also be specified".into()),
        (None, None) => Err("TLS mode requires either --pfx or both --cert and --key".into()),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    init_tracing(args.verbose);

    let bind_addr = format!("{}:{}", args.host, args.port);

    // Check for redirection configuration
    let redirection_info = if let Some(ref redirect_host) = args.redirect_host {
        Some((redirect_host.clone(), args.redirect_port))
    } else {
        None
    };

    // Create the server based on TLS mode
    // FedAuth and username/password authentication are always supported
    let server = match (&args.tls_mode, &redirection_info) {
        (TlsMode::None, Some((host, port))) => {
            info!(
                "Starting Mock TDS Server without TLS on {} with redirection to {}:{}",
                bind_addr, host, port
            );
            MockTdsServer::new_with_redirection(&bind_addr, host.clone(), *port).await?
        }
        (TlsMode::None, None) => {
            info!("Starting Mock TDS Server without TLS on {}", bind_addr);
            MockTdsServer::new(&bind_addr).await?
        }
        (TlsMode::Optional, _) => {
            let identity = load_identity(&args)?;
            info!(
                "Starting Mock TDS Server with optional TLS (TDS 7.4 mode) on {}",
                bind_addr
            );
            if redirection_info.is_some() {
                info!(
                    "Note: Redirection with TLS is not yet fully supported in CLI. Use programmatic API."
                );
            }
            MockTdsServer::new_with_tls(&bind_addr, Some(identity)).await?
        }
        (TlsMode::Strict, _) => {
            let identity = load_identity(&args)?;
            info!(
                "Starting Mock TDS Server with strict TLS (TDS 8.0 mode) on {}",
                bind_addr
            );
            if redirection_info.is_some() {
                info!(
                    "Note: Redirection with TLS is not yet fully supported in CLI. Use programmatic API."
                );
            }
            MockTdsServer::new_with_strict_tls(&bind_addr, identity).await?
        }
    };

    let actual_addr = server.local_addr();
    info!("Mock TDS Server is running on {}", actual_addr);
    println!("Mock TDS Server listening on {}", actual_addr);
    if let Some((host, port)) = &redirection_info {
        println!(
            "Redirection enabled: clients will be redirected to {}:{}",
            host, port
        );
    }
    println!("Press Ctrl+C to stop the server.");

    // Set up graceful shutdown
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let shutdown_tx = Arc::new(std::sync::Mutex::new(Some(shutdown_tx)));
    let shutdown_tx_clone = shutdown_tx.clone();

    ctrlc::set_handler(move || {
        info!("Received Ctrl+C, shutting down...");
        println!("\nShutting down...");
        // Take the sender out of the Option, send once
        match shutdown_tx_clone.lock() {
            Ok(mut shutdown_tx) => {
                if let Some(tx) = shutdown_tx.take() {
                    let _ = tx.send(());
                }
            }
            Err(error) => error!("Failed to acquire shutdown signal mutex: {}", error),
        }
    })?;

    // Run the server
    if let Err(e) = server.run_with_shutdown(shutdown_rx).await {
        error!("Server error: {}", e);
        return Err(e.into());
    }

    info!("Mock TDS Server stopped");
    Ok(())
}
