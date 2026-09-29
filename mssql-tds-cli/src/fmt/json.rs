// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Minimal structured output for `--format json`.

use serde_json::{Value, json};

use crate::cli::validate::{Encrypt, Options};
use crate::exec::runner::Output;

const SQLCMD_VERSION: &str = "1.10.0";

pub struct Document {
    connection: Value,
    output: Vec<Value>,
}

impl Document {
    pub fn new(options: &Options) -> Self {
        Self {
            connection: json!({
                "server": options.server.as_deref().unwrap_or("localhost"),
                "database": options.database.as_deref().unwrap_or("master"),
                "authentication": authentication(options),
                "encrypt": encryption_enabled(options),
            }),
            output: Vec::new(),
        }
    }

    pub fn push(&mut self, item: Output) {
        match item {
            Output::Result(_) => {
                unreachable!("text output cannot be added to a JSON document")
            }
            Output::ResultSet { columns, rows } => self.output.push(json!({
                "type": "resultSet",
                "columns": columns,
                "rows": rows,
            })),
            Output::RowsAffected(count) => self.output.push(json!({
                "type": "rowsAffected",
                "count": count,
            })),
            Output::Message(message) => self.output.push(json!({
                "type": if message.is_error() { "error" } else { "message" },
                "number": message.number,
                "state": message.state,
                "severity": message.severity,
                "message": message.text,
            })),
        }
    }

    pub fn push_client_error(&mut self, message: &str) {
        self.output.push(json!({
            "type": "error",
            "number": 0,
            "state": 0,
            "severity": 16,
            "message": message.trim_end(),
        }));
    }

    pub fn render(self, exit_code: i32) -> String {
        serde_json::to_string_pretty(&json!({
            "sqlcmd": {
                "version": SQLCMD_VERSION,
            },
            "connection": self.connection,
            "exitCode": exit_code,
            "output": self.output,
        }))
        .expect("JSON values constructed by sqlcmd are serializable")
            + "\n"
    }
}

fn authentication(options: &Options) -> &str {
    if let Some(method) = options.authentication_method.as_deref() {
        return method;
    }
    if options.use_entra_id {
        if options.trusted_connection {
            return "ActiveDirectoryIntegrated";
        }
        if options.user.is_some() && options.password.is_some() {
            return "ActiveDirectoryPassword";
        }
        if options.password.is_some() {
            return "AccessToken";
        }
        return "ActiveDirectoryDefault";
    }
    if options.trusted_connection || options.user.is_none() {
        "Integrated"
    } else {
        "SqlPassword"
    }
}

fn encryption_enabled(options: &Options) -> bool {
    !matches!(options.encrypt, Some(Encrypt::Optional))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_metadata_and_string_rows() {
        let options = Options {
            server: Some(r"(localdb)\MSSQLLocalDB".to_string()),
            database: Some("master".to_string()),
            trusted_connection: true,
            ..Options::default()
        };
        let mut document = Document::new(&options);
        document.push(Output::ResultSet {
            columns: vec!["value".to_string()],
            rows: vec![vec![Some("text".to_string())], vec![None]],
        });

        let value: Value = serde_json::from_str(&document.render(0)).unwrap();
        assert_eq!(value["sqlcmd"]["version"], SQLCMD_VERSION);
        assert_eq!(value["connection"]["server"], r"(localdb)\MSSQLLocalDB");
        assert_eq!(value["connection"]["authentication"], "Integrated");
        assert_eq!(value["connection"]["encrypt"], true);
        assert_eq!(value["output"][0]["rows"][0][0], "text");
        assert!(value["output"][0]["rows"][1][0].is_null());
    }

    #[test]
    fn reports_sql_auth_and_optional_encryption() {
        let options = Options {
            user: Some("sa".to_string()),
            password: Some("secret".to_string()),
            encrypt: Some(Encrypt::Optional),
            ..Options::default()
        };

        let value: Value = serde_json::from_str(&Document::new(&options).render(1)).unwrap();
        assert_eq!(value["connection"]["authentication"], "SqlPassword");
        assert_eq!(value["connection"]["encrypt"], false);
        assert_eq!(value["exitCode"], 1);
        assert!(value.to_string().find("secret").is_none());
    }

    #[test]
    fn includes_client_errors_in_the_document() {
        let mut document = Document::new(&Options::default());
        document.push_client_error("connection failed\r\n");

        let value: Value = serde_json::from_str(&document.render(1)).unwrap();
        assert_eq!(value["output"][0]["type"], "error");
        assert_eq!(value["output"][0]["severity"], 16);
        assert_eq!(value["output"][0]["message"], "connection failed");
    }
}
