//! One rustls crypto provider for the whole process.
//!
//! The dependency tree compiles in both of rustls's providers: aws-lc-rs
//! (reqwest 0.13, the AWS SDK) and ring (reqwest 0.12, sqlx). Crates that
//! pick a provider explicitly are fine, but those that ask rustls for the
//! process default panic when both are present, and Songbird's voice
//! WebSocket (tokio-tungstenite) is one of them: the bot crashed on its first
//! voice connection after `/radio`. `main` therefore installs aws-lc-rs (the
//! rustls default) before any TLS connection opens, and fails at startup if
//! no default is in place.

use crate::error::AppError;

pub fn install_crypto_provider() -> Result<(), AppError> {
    // Err only means a provider is already installed, which is what we want.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        return Err(AppError::Tls("no process-level rustls crypto provider"));
    }
    // The call that panicked in tokio-tungstenite, now at startup.
    std::panic::catch_unwind(rustls::ClientConfig::builder)
        .map(|_| ())
        .map_err(|_| AppError::Tls("rustls cannot build a client config"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_idempotent() {
        install_crypto_provider().unwrap();
        install_crypto_provider().unwrap();
        let _ = rustls::ClientConfig::builder();
    }

    /// Regression: open a `wss://` connection with tokio-tungstenite, exactly
    /// as Songbird does for voice, against a local listener that isn't TLS.
    /// Without an installed provider this panicked; now it must return an
    /// error.
    #[tokio::test]
    async fn voice_websocket_tls_does_not_panic() {
        install_crypto_provider().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                drop(socket);
            }
        });

        let result = tokio_tungstenite::connect_async(format!("wss://localhost:{port}/")).await;

        assert!(result.is_err(), "a non-TLS peer must fail the handshake");
    }
}
