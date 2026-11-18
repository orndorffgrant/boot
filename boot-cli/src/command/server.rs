// SPDX-License-Identifier: AGPL-3.0-or-later
use std::path::PathBuf;

use clap::Args;

use crate::domain::{
    AddServerIntent, EditServerIntent, ServerOptions, ServerSelector, TokenSource,
};

#[derive(Args, Default)]
pub struct ServerArgs {
    /// Host to listen on (defaults to 127.0.0.1)
    #[arg(long)]
    pub host: Option<String>,
    /// Port to run the server on (defaults to 46673)
    #[arg(short, long)]
    pub port: Option<u16>,
    /// Skip creating the local server entry, token, and credential
    #[arg(long)]
    pub no_provision: bool,
    /// PEM certificate for TLS
    #[arg(long, value_name = "PATH", requires = "tls_key")]
    pub tls_cert: Option<PathBuf>,
    /// PEM private key for TLS
    #[arg(long, value_name = "PATH", requires = "tls_cert")]
    pub tls_key: Option<PathBuf>,
    /// Acknowledge plaintext HTTP on a non-loopback bind
    #[arg(long)]
    pub allow_insecure_http: bool,
    /// URL to store for the local server entry
    #[arg(long, value_name = "URL")]
    pub advertise_url: Option<String>,
}

impl From<ServerArgs> for ServerOptions {
    fn from(args: ServerArgs) -> Self {
        Self {
            host: args.host,
            port: args.port,
            no_provision: args.no_provision,
            tls_cert: args.tls_cert,
            tls_key: args.tls_key,
            allow_insecure_http: args.allow_insecure_http,
            advertise_url: args.advertise_url,
        }
    }
}

#[derive(Args)]
pub struct AddServerArgs {
    /// Name of the server
    pub name: String,
    /// Connection URL (e.g., http://localhost:46673)
    pub url: String,
    /// Read the token from standard input
    #[arg(long)]
    pub token_stdin: bool,
    /// Read the token from a file
    #[arg(long, value_name = "PATH")]
    pub token_file: Option<PathBuf>,
    /// Token value (prefer --token-stdin or --token-file)
    #[arg(long, value_name = "TOKEN")]
    pub token: Option<String>,
    /// Optional PEM CA certificate for a self-signed server
    #[arg(long, value_name = "PATH")]
    pub ca_cert: Option<PathBuf>,
}

impl From<AddServerArgs> for AddServerIntent {
    fn from(args: AddServerArgs) -> Self {
        Self {
            name: args.name,
            url: args.url,
            token: TokenSource {
                stdin: args.token_stdin,
                file: args.token_file,
                value: args.token,
            },
            ca_cert: args.ca_cert,
        }
    }
}

#[derive(Args)]
pub struct ShowServerArgs {
    /// Server name (defaults to local)
    pub name: Option<String>,
}

impl From<ShowServerArgs> for ServerSelector {
    fn from(args: ShowServerArgs) -> Self {
        Self(args.name.unwrap_or_else(|| "local".to_string()))
    }
}

#[derive(Args, Default)]
pub struct EditServerArgs {
    /// Server name
    pub name: String,
    /// New connection URL
    #[arg(long, value_name = "URL")]
    pub url: Option<String>,
    /// New PEM CA certificate
    #[arg(long, value_name = "PATH", conflicts_with = "clear_ca_cert")]
    pub ca_cert: Option<PathBuf>,
    /// Remove the stored CA certificate
    #[arg(long, conflicts_with = "ca_cert")]
    pub clear_ca_cert: bool,
    /// Read the token from standard input
    #[arg(long)]
    pub token_stdin: bool,
    /// Read the token from a file
    #[arg(long, value_name = "PATH")]
    pub token_file: Option<PathBuf>,
    /// Token value (prefer --token-stdin or --token-file)
    #[arg(long, value_name = "TOKEN")]
    pub token: Option<String>,
    /// Remove the stored token
    #[arg(long, conflicts_with_all = ["token_stdin", "token_file", "token"])]
    pub clear_token: bool,
}

impl From<EditServerArgs> for EditServerIntent {
    fn from(args: EditServerArgs) -> Self {
        let ca_cert = if args.clear_ca_cert {
            Some(None)
        } else {
            args.ca_cert.map(Some)
        };
        Self {
            name: args.name,
            url: args.url,
            ca_cert,
            token: TokenSource {
                stdin: args.token_stdin,
                file: args.token_file,
                value: args.token,
            },
            clear_token: args.clear_token,
        }
    }
}

#[derive(Args)]
pub struct RemoveServerArgs {
    /// Name of the server to remove
    pub name: String,
}
