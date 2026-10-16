//! SSH local port-forward tunnels built on `russh` (pure Rust, async).
//!
//! `Tunnel::open` multiplexes every accepted local TCP connection onto a fresh
//! `direct-tcpip` channel of a single SSH session. Drivers that support custom
//! streams (Postgres) can skip the local hop with `Tunnel::open_stream`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use dbstudio_core::{Error, HostKeyPolicy, Result, SshAuth, SshConfig};
use russh::client::{self, Handle};
use russh::keys::{self, load_secret_key, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use tokio::io::copy_bidirectional;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::known_hosts::{self, apply_policy, HostKeyCheck, KnownHostsFile};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const CHANNEL_OPEN_TIMEOUT: Duration = Duration::from_secs(15);

/// A stream that is actually an SSH `direct-tcpip` channel; feed it to drivers
/// that accept `AsyncRead + AsyncWrite` (e.g. `tokio_postgres` `connect_raw`).
pub type TunnelStream = russh::ChannelStream<russh::client::Msg>;

pub struct VerifyHandler {
    host: String,
    port: u16,
    known: KnownHostsFile,
    policy: HostKeyPolicy,
}

impl client::Handler for VerifyHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = server_key else {
            tracing::warn!("rejecting SSH host certificates (not supported yet)");
            return Ok(false);
        };

        // ssh_key's Display renders "<alg> <base64> <comment>", the same
        // fields OpenSSH writes into known_hosts.
        let rendered = key.to_string();
        let mut parts = rendered.split_whitespace();
        let alg = parts.next().unwrap_or("unknown");
        let b64 = parts.next().unwrap_or_default();
        let Ok(key_bytes) =
            base64::engine::general_purpose::STANDARD.decode(b64)
        else {
            return Ok(false);
        };

        let check = self.known.check(&self.host, self.port, &key_bytes);
        match apply_policy(self.policy, check) {
            Ok(()) => {
                if check == HostKeyCheck::Unknown && self.policy == HostKeyPolicy::AcceptNew {
                    if let Err(e) = self.known.trust(&self.host, self.port, alg, &key_bytes) {
                        tracing::warn!("could not pin host key: {e}");
                    }
                }
                Ok(true)
            }
            Err(msg) => {
                tracing::error!("host key verification failed for {}:{}", self.host, self.port);
                tracing::error!("{msg}");
                Ok(false)
            }
        }
    }
}

pub struct Tunnel {
    local: SocketAddr,
    session: Arc<Handle<VerifyHandler>>,
    supervisor: JoinHandle<()>,
}

impl Tunnel {
    /// Establish an SSH session to the bastion and start a local loopback
    /// listener that forwards to `target` as seen from the bastion.
    pub async fn open(cfg: &SshConfig, target_host: &str, target_port: u16) -> Result<Self> {
        let session = Arc::new(connect_session(cfg).await?);

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(Error::ssh)?;
        let local = listener.local_addr().map_err(Error::ssh)?;

        let handle = Arc::clone(&session);
        let target_host = target_host.to_string();
        let supervisor = tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let handle = Arc::clone(&handle);
                let target_host = target_host.clone();
                tokio::spawn(async move {
                    let channel = match tokio::time::timeout(
                        CHANNEL_OPEN_TIMEOUT,
                        handle.channel_open_direct_tcpip(
                            target_host.as_str(),
                            u32::from(target_port),
                            "127.0.0.1",
                            0u32,
                        ),
                    )
                    .await
                    {
                        Ok(Ok(ch)) => ch,
                        _ => return,
                    };
                    let mut stream = channel.into_stream();
                    let _ = copy_bidirectional(&mut sock, &mut stream).await;
                });
            }
        });

        Ok(Self {
            local,
            session,
            supervisor,
        })
    }

    /// Loopback address the driver should connect to.
    pub fn local_endpoint(&self) -> (String, u16) {
        ("127.0.0.1".to_string(), self.local.port())
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Open a single forwarded channel as a stream (no local TCP hop).
    pub async fn open_stream(&self, target_host: &str, target_port: u16) -> Result<TunnelStream> {
        let channel = tokio::time::timeout(
            CHANNEL_OPEN_TIMEOUT,
            self.session.channel_open_direct_tcpip(
                target_host,
                u32::from(target_port),
                "127.0.0.1",
                0u32,
            ),
        )
        .await
        .map_err(|_| Error::Ssh("timed out opening forwarded channel".into()))?
        .map_err(|e| Error::Ssh(format!("channel open failed: {e}")))?;
        Ok(channel.into_stream())
    }

    pub async fn close(self) {
        self.supervisor.abort();
        let _ = self
            .session
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await;
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.supervisor.abort();
    }
}

async fn connect_session(cfg: &SshConfig) -> Result<Handle<VerifyHandler>> {
    let known = known_hosts::load_or_default(cfg.known_hosts_file.clone())?;

    let handler = VerifyHandler {
        host: cfg.host.clone(),
        port: cfg.port,
        known,
        policy: cfg.policy,
    };

    let client_config = Arc::new(client::Config::default());
    let connect = client::connect(client_config, (cfg.host.as_str(), cfg.port), handler);
    let mut handle = tokio::time::timeout(CONNECT_TIMEOUT, connect)
        .await
        .map_err(|_| Error::Ssh(format!("timed out connecting to {}:{}", cfg.host, cfg.port)))?
        .map_err(|e| Error::Ssh(format!("SSH connect failed: {e}")))?;

    authenticate(&mut handle, cfg).await?;
    Ok(handle)
}

async fn authenticate(handle: &mut Handle<VerifyHandler>, cfg: &SshConfig) -> Result<()> {
    let user = cfg.username.as_str();
    let auth_result = match &cfg.auth {
        SshAuth::Password { password } => handle
            .authenticate_password(user, password.expose())
            .await
            .map_err(|e| Error::Auth(format!("SSH password auth failed: {e}")))?,

        SshAuth::PrivateKey { path, passphrase } => {
            let key = load_secret_key(path, passphrase.as_ref().map(|p| p.expose()))
                .map_err(|e| Error::Auth(format!("could not load SSH key {path:?}: {e}")))?;
            let rsa_hash = match handle.best_supported_rsa_hash().await {
                Ok(Some(alg)) => alg,
                Ok(None) | Err(_) => Some(keys::HashAlg::Sha256),
            };
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash);
            handle
                .authenticate_publickey(user, key)
                .await
                .map_err(|e| Error::Auth(format!("SSH public key auth failed: {e}")))?
        }

        SshAuth::Agent => {
            return Err(Error::Auth(
                "SSH agent auth is not implemented yet; use a key file or password".into(),
            ));
        }
    };

    match auth_result {
        client::AuthResult::Success => Ok(()),
        client::AuthResult::Failure {
            remaining_methods,
            partial_success,
        } => Err(Error::Auth(format!(
            "SSH authentication denied (partial={partial_success}, remaining={remaining_methods:?})"
        ))),
    }
}
