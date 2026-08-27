// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The `-?` syntax summary.
//!
//! Layout, column alignment and wording are copied from ODBC sqlcmd so that
//! scripts scraping the help text keep working. The banner's version line is
//! the only part that differs, and the differential tests normalize it.

use crate::messages::EOL;

const BANNER_PRODUCT: &str = "Microsoft (R) SQL Server Command Line Tool";
const BANNER_COPYRIGHT: &str = "Copyright (C) 2025 Microsoft Corporation. All rights reserved.";

const SYNTAX: &str = "\
usage: Sqlcmd            [-U login id]          [-P password]
  [-S server]            [-H hostname]          [-E trusted connection]
  [-N[s|m|o] Encrypt Connection]
  [-C Trust Server Certificate]
  [-F Hostname in certificate]
  [-d use database name] [-l login timeout]     [-t query timeout]
  [-h headers]           [-s colseparator]      [-w screen width]
  [-a packetsize]        [-e echo input]        [-I Enable Quoted Identifiers]
  [-c cmdend]            [-L[c] list servers[clean output]]
  [-q \"cmdline query\"]   [-Q \"cmdline query\" and exit]
  [-m errorlevel]        [-V severitylevel]     [-W remove trailing spaces]
  [-u unicode output]    [-r[0|1] msgs to stderr]
  [-i inputfile]         [-o outputfile]        [-z new password]
  [-f <codepage> | i:<codepage>[,o:<codepage>]] [-Z new password and exit]
  [-k[1|2] remove[replace] control characters]
  [-y variable length type display width]
  [-Y fixed length type display width]
  [-p[1] print statistics[colon format]]
  [-R use client regional setting]
  [-K application intent]
  [-M multisubnet failover]
  [-b On error batch abort]
  [-v var = \"value\"...]  [-A dedicated admin connection]
  [-X[1] disable commands, startup script, environment variables [and exit]]
  [-x disable variable substitution]
  [-j Print raw error messages]
  [-g enable column encryption]
  [-G use Microsoft Entra ID for authentication]
  [-? show syntax summary]
";

/// The banner printed ahead of the syntax summary.
pub fn banner() -> String {
    format!(
        "{BANNER_PRODUCT}{EOL}Version {} NT{EOL}{BANNER_COPYRIGHT}{EOL}",
        env!("CARGO_PKG_VERSION")
    )
}

/// The full `-?` output: banner, blank line, then the syntax summary.
pub fn usage() -> String {
    let mut out = banner();
    out.push_str(EOL);
    for line in SYNTAX.lines() {
        out.push_str(line);
        out.push_str(EOL);
    }
    out
}
