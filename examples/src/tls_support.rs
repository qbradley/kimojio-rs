// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use openssl::ssl::{SslAcceptor, SslConnector, SslFiletype, SslMethod, SslVerifyMode};

#[derive(Clone, Debug)]
pub enum TlsMode {
    Plain,
    Tls { cert: PathBuf, key: PathBuf },
}

impl TlsMode {
    pub fn is_tls(&self) -> bool {
        matches!(self, Self::Tls { .. })
    }

    pub fn from_args(tls: bool, cert: &Option<PathBuf>, key: &Option<PathBuf>) -> Result<Self> {
        if !tls {
            return Ok(Self::Plain);
        }

        let cert = cert
            .clone()
            .context("--cert is required when --tls is enabled")?;
        let key = key
            .clone()
            .context("--key is required when --tls is enabled")?;
        if !cert.exists() {
            bail!("certificate file does not exist: {}", cert.display());
        }
        if !key.exists() {
            bail!("private key file does not exist: {}", key.display());
        }

        Ok(Self::Tls { cert, key })
    }
}

pub fn server_context(cert: &Path, key: &Path) -> Result<openssl::ssl::SslContext> {
    Ok(server_acceptor(cert, key)?.into_context())
}

pub fn server_acceptor(cert: &Path, key: &Path) -> Result<SslAcceptor> {
    let mut acceptor_builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server())
        .context("create TLS acceptor")?;
    acceptor_builder
        .set_certificate_file(cert, SslFiletype::PEM)
        .with_context(|| format!("load certificate {}", cert.display()))?;
    acceptor_builder
        .set_private_key_file(key, SslFiletype::PEM)
        .with_context(|| format!("load private key {}", key.display()))?;
    acceptor_builder.check_private_key().with_context(|| {
        format!(
            "private key {} does not match certificate {}",
            key.display(),
            cert.display()
        )
    })?;
    Ok(acceptor_builder.build())
}

pub fn client_connector(insecure: bool, ca_cert: Option<&Path>) -> Result<SslConnector> {
    let mut connector =
        SslConnector::builder(SslMethod::tls_client()).context("create TLS connector")?;
    if insecure {
        connector.set_verify(SslVerifyMode::NONE);
    } else if let Some(ca_cert) = ca_cert {
        connector
            .set_ca_file(ca_cert)
            .with_context(|| format!("load CA certificate {}", ca_cert.display()))?;
    }
    Ok(connector.build())
}
