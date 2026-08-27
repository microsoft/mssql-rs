// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The `sqlconfig` file: contexts, endpoints and users.
//!
//! A *context* names an endpoint and optionally a user, and one context is
//! current at a time. `sqlcmd query` and friends work against whatever that is,
//! so a machine can carry several servers and switch between them by name.
//!
//! The schema and field order match go-sqlcmd's, since the two tools read and
//! write the same file.

use std::path::{Path, PathBuf};

use super::yaml::{self, Yaml};

/// The version stamp go-sqlcmd writes.
const VERSION: &str = "v1";

pub const DEFAULT_PORT: u16 = 1433;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub name: String,
    pub address: String,
    pub port: u16,
    /// Set when the endpoint is a container this tool created.
    pub container: Option<Container>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    pub id: String,
    pub image: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    pub name: String,
    pub endpoint: String,
    pub user: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub authentication_type: String,
    pub username: String,
    pub password: String,
    pub password_encryption: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SqlConfig {
    /// As read from the file. Empty when there was no file, which `config view`
    /// shows verbatim; a save always stamps the current version.
    pub version: String,
    pub endpoints: Vec<Endpoint>,
    pub contexts: Vec<Context>,
    pub current_context: String,
    pub users: Vec<User>,
}

/// Where the file lives unless `--sqlconfig` says otherwise.
pub fn default_path() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".sqlcmd").join("sqlconfig")
}

impl SqlConfig {
    /// Reads the file, treating a missing one as empty — the first `add-*`
    /// command on a machine has nothing to read.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let doc =
            yaml::parse(&text).map_err(|e| format!("{} is not valid YAML: {e}", path.display()))?;
        Ok(Self::from_yaml(&doc))
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(path, yaml::emit(&self.to_yaml_file()))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    fn from_yaml(doc: &Yaml) -> Self {
        let endpoints = doc
            .get("endpoints")
            .map(Yaml::as_list)
            .unwrap_or_default()
            .iter()
            .map(|item| {
                let detail = item.get("endpoint");
                Endpoint {
                    name: item.str_at("name").to_string(),
                    address: detail
                        .map(|d| d.str_at("address"))
                        .unwrap_or("")
                        .to_string(),
                    port: detail
                        .map(|d| d.str_at("port"))
                        .unwrap_or("")
                        .parse()
                        .unwrap_or(DEFAULT_PORT),
                    container: item.get("asset").and_then(|a| a.get("container")).map(|c| {
                        Container {
                            id: c.str_at("id").to_string(),
                            image: c.str_at("image").to_string(),
                        }
                    }),
                }
            })
            .collect();

        let contexts = doc
            .get("contexts")
            .map(Yaml::as_list)
            .unwrap_or_default()
            .iter()
            .map(|item| {
                let detail = item.get("context");
                Context {
                    name: item.str_at("name").to_string(),
                    endpoint: detail
                        .map(|d| d.str_at("endpoint"))
                        .unwrap_or("")
                        .to_string(),
                    user: detail
                        .and_then(|d| d.get("user"))
                        .and_then(Yaml::as_str)
                        .filter(|u| !u.is_empty())
                        .map(str::to_string),
                }
            })
            .collect();

        let users = doc
            .get("users")
            .map(Yaml::as_list)
            .unwrap_or_default()
            .iter()
            .map(|item| {
                let basic = item.get("basic-auth");
                User {
                    name: item.str_at("name").to_string(),
                    authentication_type: item.str_at("authentication-type").to_string(),
                    username: basic
                        .map(|b| b.str_at("username"))
                        .unwrap_or("")
                        .to_string(),
                    password: basic
                        .map(|b| b.str_at("password"))
                        .unwrap_or("")
                        .to_string(),
                    password_encryption: basic
                        .map(|b| b.str_at("password-encryption"))
                        .unwrap_or("")
                        .to_string(),
                }
            })
            .collect();

        SqlConfig {
            version: doc.str_at("version").to_string(),
            endpoints,
            contexts,
            current_context: doc.str_at("currentcontext").to_string(),
            users,
        }
    }

    fn endpoint_entry(endpoint: &Endpoint) -> Yaml {
        let mut entry = vec![(
            "endpoint".to_string(),
            Yaml::Map(vec![
                ("address".to_string(), Yaml::scalar(&endpoint.address)),
                ("port".to_string(), Yaml::scalar(endpoint.port.to_string())),
            ]),
        )];
        if let Some(container) = &endpoint.container {
            entry.push((
                "asset".to_string(),
                Yaml::Map(vec![(
                    "container".to_string(),
                    Yaml::Map(vec![
                        ("id".to_string(), Yaml::scalar(&container.id)),
                        ("image".to_string(), Yaml::scalar(&container.image)),
                    ]),
                )]),
            ));
        }
        entry.push(("name".to_string(), Yaml::scalar(&endpoint.name)));
        Yaml::Map(entry)
    }

    fn context_entry(context: &Context) -> Yaml {
        let mut detail = vec![("endpoint".to_string(), Yaml::scalar(&context.endpoint))];
        if let Some(user) = &context.user {
            detail.push(("user".to_string(), Yaml::scalar(user)));
        }
        Yaml::Map(vec![
            ("context".to_string(), Yaml::Map(detail)),
            ("name".to_string(), Yaml::scalar(&context.name)),
        ])
    }

    /// The stored password is base64; `--raw` shows what it decodes to, which
    /// is what the caller would type.
    fn user_entry(user: &User, redact: bool) -> Yaml {
        let password = if redact {
            "REDACTED".to_string()
        } else {
            crate::modern::config_cmds::base64::decode_text(&user.password)
        };
        Yaml::Map(vec![
            ("name".to_string(), Yaml::scalar(&user.name)),
            (
                "authentication-type".to_string(),
                Yaml::scalar(&user.authentication_type),
            ),
            (
                "basic-auth".to_string(),
                Yaml::Map(vec![
                    ("username".to_string(), Yaml::scalar(&user.username)),
                    (
                        "password-encryption".to_string(),
                        Yaml::scalar(&user.password_encryption),
                    ),
                    ("password".to_string(), Yaml::scalar(password)),
                ]),
            ),
        ])
    }

    fn list<T>(items: &[T], render: impl Fn(&T) -> Yaml) -> Yaml {
        Yaml::List(items.iter().map(render).collect())
    }

    /// The on-disk form, whose keys go-sqlcmd writes in alphabetical order and
    /// whose password stays in its stored encoding.
    fn to_yaml_file(&self) -> Yaml {
        Yaml::Map(vec![
            (
                "contexts".to_string(),
                Self::list(&self.contexts, Self::context_entry),
            ),
            (
                "currentcontext".to_string(),
                Yaml::scalar(&self.current_context),
            ),
            (
                "endpoints".to_string(),
                Self::list(&self.endpoints, Self::endpoint_entry),
            ),
            (
                "users".to_string(),
                Self::list(&self.users, |u| {
                    Yaml::Map(vec![
                        ("name".to_string(), Yaml::scalar(&u.name)),
                        (
                            "authentication-type".to_string(),
                            Yaml::scalar(&u.authentication_type),
                        ),
                        (
                            "basic-auth".to_string(),
                            Yaml::Map(vec![
                                ("username".to_string(), Yaml::scalar(&u.username)),
                                (
                                    "password-encryption".to_string(),
                                    Yaml::scalar(&u.password_encryption),
                                ),
                                ("password".to_string(), Yaml::scalar(&u.password)),
                            ]),
                        ),
                    ])
                }),
            ),
            ("version".to_string(), Yaml::scalar(VERSION)),
        ])
    }

    /// The form `config view` prints, whose keys follow the struct rather than
    /// the alphabet.
    pub fn to_yaml_view(&self, raw: bool) -> Yaml {
        Yaml::Map(vec![
            ("version".to_string(), Yaml::scalar(&self.version)),
            (
                "endpoints".to_string(),
                Self::list(&self.endpoints, Self::endpoint_entry),
            ),
            (
                "contexts".to_string(),
                Self::list(&self.contexts, Self::context_entry),
            ),
            (
                "currentcontext".to_string(),
                Yaml::scalar(&self.current_context),
            ),
            (
                "users".to_string(),
                Self::list(&self.users, |u| Self::user_entry(u, !raw)),
            ),
        ])
    }

    pub fn find_endpoint(&self, name: &str) -> Option<&Endpoint> {
        self.endpoints.iter().find(|e| e.name == name)
    }

    pub fn find_context(&self, name: &str) -> Option<&Context> {
        self.contexts.iter().find(|c| c.name == name)
    }

    pub fn find_user(&self, name: &str) -> Option<&User> {
        self.users.iter().find(|u| u.name == name)
    }

    /// The current context together with what it points at.
    pub fn current(&self) -> Option<(&Context, &Endpoint, Option<&User>)> {
        let context = self.find_context(&self.current_context)?;
        let endpoint = self.find_endpoint(&context.endpoint)?;
        let user = context.user.as_deref().and_then(|n| self.find_user(n));
        Some((context, endpoint, user))
    }

    /// Appends `2`, `3`, … until the name is free, as go-sqlcmd does when a
    /// default name collides.
    pub fn unique_name(&self, base: &str, kind: Kind) -> String {
        let taken = |name: &str| match kind {
            Kind::Endpoint => self.find_endpoint(name).is_some(),
            Kind::Context => self.find_context(name).is_some(),
            Kind::User => self.find_user(name).is_some(),
        };
        if !taken(base) {
            return base.to_string();
        }
        (2..)
            .map(|n| format!("{base}{n}"))
            .find(|c| !taken(c))
            .expect("an unused name exists")
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Endpoint,
    Context,
    User,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SqlConfig {
        SqlConfig {
            version: VERSION.to_string(),
            endpoints: vec![Endpoint {
                name: "ep1".to_string(),
                address: "localhost".to_string(),
                port: 1433,
                container: None,
            }],
            contexts: vec![Context {
                name: "ctx1".to_string(),
                endpoint: "ep1".to_string(),
                user: Some("u1".to_string()),
            }],
            current_context: "ctx1".to_string(),
            users: vec![User {
                name: "u1".to_string(),
                authentication_type: "basic".to_string(),
                username: "sa".to_string(),
                password: "c2VjcmV0".to_string(),
                password_encryption: "none".to_string(),
            }],
        }
    }

    #[test]
    fn the_saved_file_matches_what_go_sqlcmd_writes() {
        let empty = SqlConfig::default();
        assert_eq!(
            yaml::emit(&empty.to_yaml_file()),
            "contexts: []\ncurrentcontext: \"\"\nendpoints: []\nusers: []\nversion: v1\n"
        );
    }

    #[test]
    fn a_config_survives_a_round_trip_through_yaml() {
        let config = sample();
        let text = yaml::emit(&config.to_yaml_file());
        let parsed = SqlConfig::from_yaml(&yaml::parse(&text).unwrap());
        assert_eq!(parsed, config);
    }

    #[test]
    fn a_container_endpoint_survives_a_round_trip() {
        let mut config = sample();
        config.endpoints[0].container = Some(Container {
            id: "abc123".to_string(),
            image: "mcr.microsoft.com/mssql/server:2022-latest".to_string(),
        });
        let text = yaml::emit(&config.to_yaml_file());
        let parsed = SqlConfig::from_yaml(&yaml::parse(&text).unwrap());
        assert_eq!(parsed, config);
    }

    #[test]
    fn view_redacts_the_password_unless_asked_not_to() {
        let config = sample();
        assert!(yaml::emit(&config.to_yaml_view(false)).contains("password: REDACTED"));
        assert!(yaml::emit(&config.to_yaml_view(true)).contains("password: secret"));
    }

    #[test]
    fn view_orders_its_keys_the_way_go_sqlcmd_does() {
        let text = yaml::emit(&sample().to_yaml_view(false));
        let keys: Vec<&str> = text
            .lines()
            .filter(|l| !l.starts_with(' ') && !l.starts_with('-'))
            .filter_map(|l| l.split(':').next())
            .collect();
        assert_eq!(
            keys,
            vec![
                "version",
                "endpoints",
                "contexts",
                "currentcontext",
                "users"
            ]
        );
    }

    #[test]
    fn the_version_shown_is_the_one_read_rather_than_the_one_written() {
        // A file that has never been saved has no version, and `config view`
        // says so instead of claiming the current one.
        let fresh = SqlConfig::default();
        assert!(yaml::emit(&fresh.to_yaml_view(false)).starts_with("version: \"\""));
        assert!(yaml::emit(&fresh.to_yaml_file()).contains("version: v1"));
    }

    #[test]
    fn a_missing_file_reads_as_an_empty_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = SqlConfig::load(&dir.path().join("absent")).unwrap();
        assert_eq!(config, SqlConfig::default());
    }

    #[test]
    fn a_colliding_name_gets_a_number() {
        let config = sample();
        assert_eq!(config.unique_name("ep1", Kind::Endpoint), "ep12");
        assert_eq!(config.unique_name("other", Kind::Endpoint), "other");
    }

    #[test]
    fn current_resolves_the_endpoint_and_user() {
        let config = sample();
        let (context, endpoint, user) = config.current().unwrap();
        assert_eq!(context.name, "ctx1");
        assert_eq!(endpoint.address, "localhost");
        assert_eq!(user.unwrap().username, "sa");
    }

    #[test]
    fn a_context_without_a_user_still_resolves() {
        let mut config = sample();
        config.contexts[0].user = None;
        let (_, _, user) = config.current().unwrap();
        assert!(user.is_none());
    }
}
