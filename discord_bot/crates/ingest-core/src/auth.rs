//! Application-level caller identity (ADR 0004 §4). Cloud Run IAM only says "may invoke this
//! service"; the app verifies the Google ID token itself and maps its `email` to a route or a
//! producer.
//!
//! Callers must send `Authorization: Bearer <ID token>`. Cloud Run forwards that header
//! unchanged, but strips the signature from `X-Serverless-Authorization`, so a token that
//! arrives without a verifiable signature is rejected rather than trusted.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use tokio::sync::RwLock;

pub const GOOGLE_JWKS_URL: &str = "https://www.googleapis.com/oauth2/v3/certs";
const GOOGLE_ISSUERS: [&str; 2] = ["https://accounts.google.com", "accounts.google.com"];
const JWKS_TTL: Duration = Duration::from_secs(60 * 60);
/// At most one refetch per this interval when a token names an unknown key.
const JWKS_MIN_REFRESH: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub email: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthError {
    #[error("missing bearer token")]
    Missing,
    #[error("invalid token: {0}")]
    Invalid(String),
    #[error("signing keys unavailable: {0}")]
    Keys(String),
}

#[async_trait]
pub trait KeySource: Send + Sync {
    /// The current key set; `force` refetches (rate-limited by the implementation).
    async fn keys(&self, force: bool) -> Result<JwkSet, String>;
}

/// Google's published keys, cached for an hour.
pub struct GoogleKeys {
    client: reqwest::Client,
    url: String,
    cache: RwLock<Option<(JwkSet, Instant)>>,
}

impl GoogleKeys {
    pub fn new(url: impl Into<String>) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
            url: url.into(),
            cache: RwLock::new(None),
        })
    }
}

#[async_trait]
impl KeySource for GoogleKeys {
    async fn keys(&self, force: bool) -> Result<JwkSet, String> {
        if let Some((set, at)) = self.cache.read().await.as_ref() {
            let fresh = at.elapsed() < JWKS_TTL;
            let too_soon = at.elapsed() < JWKS_MIN_REFRESH;
            if (fresh && !force) || too_soon {
                return Ok(set.clone());
            }
        }
        let set: JwkSet = self
            .client
            .get(&self.url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        *self.cache.write().await = Some((set.clone(), Instant::now()));
        Ok(set)
    }
}

/// A fixed key set (tests, local development).
pub struct StaticKeys(pub JwkSet);

#[async_trait]
impl KeySource for StaticKeys {
    async fn keys(&self, _force: bool) -> Result<JwkSet, String> {
        Ok(self.0.clone())
    }
}

#[derive(Deserialize)]
struct Claims {
    email: Option<String>,
    email_verified: Option<bool>,
}

pub struct IdTokenVerifier {
    keys: Arc<dyn KeySource>,
    audiences: Vec<String>,
    issuers: Vec<String>,
}

impl IdTokenVerifier {
    /// Google-issued tokens for any of `audiences` (the service URLs callers use).
    pub fn google(keys: Arc<dyn KeySource>, audiences: Vec<String>) -> Self {
        Self {
            keys,
            audiences,
            issuers: GOOGLE_ISSUERS.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Verifies an `Authorization` header value.
    pub async fn verify(&self, header: Option<&str>) -> Result<Caller, AuthError> {
        let token = header
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or(AuthError::Missing)?;
        let head =
            jsonwebtoken::decode_header(token).map_err(|e| AuthError::Invalid(e.to_string()))?;
        if head.alg != Algorithm::RS256 {
            return Err(AuthError::Invalid("unexpected algorithm".into()));
        }
        let kid = head
            .kid
            .ok_or_else(|| AuthError::Invalid("no key id".into()))?;
        let mut set = self.keys.keys(false).await.map_err(AuthError::Keys)?;
        if set.find(&kid).is_none() {
            set = self.keys.keys(true).await.map_err(AuthError::Keys)?;
        }
        let jwk = set
            .find(&kid)
            .ok_or_else(|| AuthError::Invalid("unknown key id".into()))?;
        let key = DecodingKey::from_jwk(jwk).map_err(|e| AuthError::Invalid(e.to_string()))?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&self.audiences);
        validation.set_issuer(&self.issuers);
        validation.set_required_spec_claims(&["exp", "iat", "aud", "iss"]);
        validation.leeway = 30;
        let data = jsonwebtoken::decode::<Claims>(token, &key, &validation)
            .map_err(|e| AuthError::Invalid(e.to_string()))?;
        let email = data
            .claims
            .email
            .filter(|e| !e.is_empty())
            .ok_or_else(|| AuthError::Invalid("no email claim".into()))?;
        if data.claims.email_verified != Some(true) {
            return Err(AuthError::Invalid("email not verified".into()));
        }
        Ok(Caller { email })
    }
}

/// Test helpers: an RSA key pair with a matching JWK set, and token minting.
#[doc(hidden)]
pub mod testing {
    use jsonwebtoken::jwk::JwkSet;
    use jsonwebtoken::{Algorithm, EncodingKey, Header};
    use serde_json::json;

    /// Test-only 2048-bit RSA key (never used outside tests).
    pub const PRIVATE_KEY_PEM: &str = include_str!("../testdata/test-rsa.pem");
    pub const KID: &str = "test-key-1";

    pub fn jwks() -> JwkSet {
        serde_json::from_str(include_str!("../testdata/test-jwks.json")).unwrap()
    }

    /// A Google-shaped ID token signed with the test key.
    pub fn token(email: &str, aud: &str, verified: bool, exp_offset: i64) -> String {
        let now = chrono::Utc::now().timestamp();
        let claims = json!({
            "iss": "https://accounts.google.com",
            "aud": aud,
            "sub": "1234567890",
            "email": email,
            "email_verified": verified,
            "iat": now - 10,
            "exp": now + exp_offset,
        });
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(KID.into());
        jsonwebtoken::encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_pem(PRIVATE_KEY_PEM.as_bytes()).unwrap(),
        )
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    const AUD: &str = "https://ingest-123.asia-southeast1.run.app";

    fn verifier() -> IdTokenVerifier {
        IdTokenVerifier::google(Arc::new(StaticKeys(jwks())), vec![AUD.into()])
    }

    fn bearer(t: &str) -> String {
        format!("Bearer {t}")
    }

    #[tokio::test]
    async fn accepts_a_valid_google_token() {
        let t = token("bot@p.iam.gserviceaccount.com", AUD, true, 300);
        let caller = verifier().verify(Some(&bearer(&t))).await.unwrap();
        assert_eq!(caller.email, "bot@p.iam.gserviceaccount.com");
    }

    #[tokio::test]
    async fn rejects_wrong_audience_expired_unverified_and_missing() {
        let v = verifier();
        let wrong_aud = token("a@x", "https://other.run.app", true, 300);
        let expired = token("a@x", AUD, true, -120);
        let unverified = token("a@x", AUD, false, 300);
        for t in [wrong_aud, expired, unverified] {
            assert!(matches!(
                v.verify(Some(&bearer(&t))).await,
                Err(AuthError::Invalid(_))
            ));
        }
        assert_eq!(v.verify(None).await, Err(AuthError::Missing));
        assert_eq!(v.verify(Some("Basic abc")).await, Err(AuthError::Missing));
    }

    /// What Cloud Run passes on from `X-Serverless-Authorization`: the signature removed.
    #[tokio::test]
    async fn rejects_a_token_whose_signature_was_stripped() {
        let t = token("a@x", AUD, true, 300);
        let (unsigned, _) = t.rsplit_once('.').unwrap();
        for forged in [format!("{unsigned}."), unsigned.to_owned()] {
            assert!(matches!(
                verifier().verify(Some(&bearer(&forged))).await,
                Err(AuthError::Invalid(_))
            ));
        }
    }

    #[tokio::test]
    async fn rejects_a_tampered_payload_and_alg_none() {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let t = token("a@x", AUD, true, 300);
        let parts: Vec<&str> = t.split('.').collect();
        let mut claims: serde_json::Value =
            serde_json::from_slice(&b64.decode(parts[1]).unwrap()).unwrap();
        claims["email"] = "admin@x".into();
        let tampered = format!(
            "{}.{}.{}",
            parts[0],
            b64.encode(claims.to_string()),
            parts[2]
        );
        assert!(verifier().verify(Some(&bearer(&tampered))).await.is_err());

        let none_header = b64.encode(r#"{"alg":"none","typ":"JWT","kid":"test-key-1"}"#);
        let alg_none = format!("{none_header}.{}.", parts[1]);
        assert!(verifier().verify(Some(&bearer(&alg_none))).await.is_err());
    }
}
