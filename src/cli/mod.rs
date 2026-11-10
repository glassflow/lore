//! The CLI side of lore: commands that talk to a server over HTTP.

use anyhow::Result;
use clap::Subcommand;
use serde::Serialize;

pub mod ask;
pub mod client;
pub mod doc;
pub mod otlp;
pub mod repo;
pub mod search;

pub use ask::{AskArgs, run_ask};
pub use client::{Auth, Client};
pub use doc::{DocCommand, run_doc};
pub use otlp::{OtlpCommand, run_otlp};
pub use repo::{RepoCommand, run_repo};
pub use search::{SearchArgs, run_search};

#[derive(Subcommand)]
pub enum AdminCommand {
    /// Manage tenants
    Tenant {
        #[command(subcommand)]
        command: TenantCommand,
    },
    /// Manage API keys
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// Manage sources
    Source {
        #[command(subcommand)]
        command: SourceCommand,
    },
}

#[derive(Subcommand)]
pub enum TenantCommand {
    /// Create a tenant
    Create {
        /// 1 to 64 characters of a-z, 0-9 and -
        #[arg(long)]
        name: String,
    },
    /// List tenants
    List,
    /// Delete a tenant with all its sources, documents, keys and repo directories
    Delete {
        /// Tenant id
        id: String,
    },
}

#[derive(Subcommand)]
pub enum SourceCommand {
    /// Delete a source with its documents and repo directory
    Delete {
        /// Source id
        id: String,
    },
}

#[derive(Subcommand)]
pub enum KeyCommand {
    /// Create an API key. The key is printed once and cannot be shown again
    Create {
        /// Tenant id
        #[arg(long)]
        tenant: String,
        /// Comma-separated scopes (ingest, read). Defaults to both
        #[arg(long, value_delimiter = ',')]
        scopes: Option<Vec<String>>,
    },
    /// List a tenant's keys (prefixes only)
    List {
        /// Tenant id
        #[arg(long)]
        tenant: String,
    },
    /// Revoke a key
    Revoke {
        /// Key id
        id: String,
    },
}

#[derive(Serialize)]
struct CreateTenantBody<'a> {
    name: &'a str,
}

#[derive(Serialize)]
struct CreateKeyBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    scopes: Option<&'a [String]>,
}

#[derive(Serialize)]
struct Deleted<'a> {
    id: &'a str,
    deleted: bool,
}

#[derive(Serialize)]
struct Revoked<'a> {
    id: &'a str,
    revoked: bool,
}

pub fn print_json<T: Serialize + ?Sized>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub async fn run_admin(client: &Client, command: AdminCommand) -> Result<()> {
    use serde_json::Value;
    match command {
        AdminCommand::Tenant { command } => match command {
            TenantCommand::Create { name } => {
                let v: Value = client
                    .post_json(
                        "/v1/admin/tenants",
                        Auth::Admin,
                        &CreateTenantBody { name: &name },
                    )
                    .await?;
                print_json(&v)
            }
            TenantCommand::List => {
                let v: Value = client.get_json("/v1/admin/tenants", Auth::Admin).await?;
                print_json(&v)
            }
            TenantCommand::Delete { id } => {
                client
                    .delete(&format!("/v1/admin/tenants/{id}"), Auth::Admin)
                    .await?;
                print_json(&Deleted {
                    id: &id,
                    deleted: true,
                })
            }
        },
        AdminCommand::Source { command } => match command {
            SourceCommand::Delete { id } => {
                client
                    .delete(&format!("/v1/admin/sources/{id}"), Auth::Admin)
                    .await?;
                print_json(&Deleted {
                    id: &id,
                    deleted: true,
                })
            }
        },
        AdminCommand::Key { command } => match command {
            KeyCommand::Create { tenant, scopes } => {
                let body = CreateKeyBody {
                    scopes: scopes.as_deref(),
                };
                let v: Value = client
                    .post_json(
                        &format!("/v1/admin/tenants/{tenant}/keys"),
                        Auth::Admin,
                        &body,
                    )
                    .await?;
                print_json(&v)
            }
            KeyCommand::List { tenant } => {
                let v: Value = client
                    .get_json(&format!("/v1/admin/tenants/{tenant}/keys"), Auth::Admin)
                    .await?;
                print_json(&v)
            }
            KeyCommand::Revoke { id } => {
                client
                    .delete(&format!("/v1/admin/keys/{id}"), Auth::Admin)
                    .await?;
                print_json(&Revoked {
                    id: &id,
                    revoked: true,
                })
            }
        },
    }
}

pub async fn run_me(client: &Client) -> Result<()> {
    let v: serde_json::Value = client.get_json("/v1/me", Auth::Key).await?;
    print_json(&v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_body_omits_scopes_by_default() {
        let body = CreateKeyBody { scopes: None };
        assert_eq!(serde_json::to_string(&body).unwrap(), "{}");
        let s = vec!["read".to_string()];
        let body = CreateKeyBody { scopes: Some(&s) };
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"scopes":["read"]}"#
        );
    }
}
