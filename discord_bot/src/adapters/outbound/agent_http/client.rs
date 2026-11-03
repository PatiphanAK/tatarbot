use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{StatusCode, Url};

use super::dto::{ChatRequestDto, ChatResponseDto, ErrorDto};
use crate::application::ports::{AgentError, AgentPort};
use crate::domain::{ChatRequest, ChatResponse};

/// Cloud Run checks this header instead of `Authorization` when both are present, so
/// the runtime's own bearer key keeps working behind IAM.
const SERVERLESS_AUTHORIZATION: &str = "x-serverless-authorization";

pub struct HttpAgentClient {
    http: reqwest::Client,
    chat_url: Url,
    api_key: Option<String>,
    id_token_file: Option<PathBuf>,
}

impl HttpAgentClient {
    pub fn new(
        base_url: &Url,
        api_key: Option<String>,
        id_token_file: Option<PathBuf>,
        timeout: Duration,
    ) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("tatarbot/", env!("CARGO_PKG_VERSION")))
            .build()?;
        // Ensure a trailing slash so `join` appends instead of replacing the
        // last path segment (e.g. `http://host/api` -> `http://host/api/v1/chat`).
        let mut base = base_url.clone();
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let chat_url = base.join("v1/chat").expect("static path is valid");
        Ok(Self {
            http,
            chat_url,
            api_key,
            id_token_file,
        })
    }

    async fn chat_request(
        &self,
        request: &ChatRequest,
    ) -> Result<reqwest::RequestBuilder, AgentError> {
        let mut builder = self
            .http
            .post(self.chat_url.clone())
            .json(&ChatRequestDto::from(request));
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }
        if let Some(path) = &self.id_token_file {
            let token = read_id_token(path).await?;
            builder = builder.header(SERVERLESS_AUTHORIZATION, format!("Bearer {token}"));
        }
        Ok(builder)
    }
}

/// The current ID token. A missing or empty file is an operator problem (the host's
/// refresh timer), reported like an unreachable runtime.
async fn read_id_token(path: &Path) -> Result<String, AgentError> {
    let token = tokio::fs::read_to_string(path).await.map_err(|e| {
        tracing::error!(path = %path.display(), error = %e, "cannot read the runtime ID token");
        AgentError::Unavailable(format!("ID token unavailable: {e}"))
    })?;
    let token = token.trim();
    if token.is_empty() || token.contains(char::is_whitespace) {
        tracing::error!(path = %path.display(), "runtime ID token file is empty or malformed");
        return Err(AgentError::Unavailable("ID token unavailable".to_owned()));
    }
    Ok(token.to_owned())
}

#[async_trait]
impl AgentPort for HttpAgentClient {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, AgentError> {
        let response = self
            .chat_request(&request)
            .await?
            .send()
            .await
            .map_err(map_transport_error)?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(map_status(status, &body));
        }

        let dto: ChatResponseDto = response
            .json()
            .await
            .map_err(|e| AgentError::InvalidResponse(e.to_string()))?;
        ChatResponse::try_from(dto).map_err(|e| AgentError::InvalidResponse(e.to_string()))
    }
}

fn map_transport_error(err: reqwest::Error) -> AgentError {
    if err.is_timeout() {
        AgentError::Timeout
    } else {
        AgentError::Unavailable(err.to_string())
    }
}

fn map_status(status: StatusCode, body: &str) -> AgentError {
    let detail = serde_json::from_str::<ErrorDto>(body)
        .map(|e| e.error)
        .unwrap_or_else(|_| status.to_string());
    match status {
        StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => AgentError::Timeout,
        // A bad AGENT_API_KEY is an operator problem, not the user's fault.
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            tracing::error!(%status, %detail, "agent runtime rejected credentials; check AGENT_API_KEY and, on Cloud Run, the ID token and run.invoker");
            AgentError::Unavailable(detail)
        }
        StatusCode::TOO_MANY_REQUESTS => AgentError::Unavailable(detail),
        s if s.is_client_error() => AgentError::Rejected(detail),
        _ => AgentError::Unavailable(detail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::domain::ConversationId;

    fn client(base: &str, api_key: Option<&str>) -> HttpAgentClient {
        client_with_token(base, api_key, None)
    }

    fn client_with_token(
        base: &str,
        api_key: Option<&str>,
        id_token_file: Option<PathBuf>,
    ) -> HttpAgentClient {
        HttpAgentClient::new(
            &Url::parse(base).unwrap(),
            api_key.map(str::to_owned),
            id_token_file,
            Duration::from_secs(1),
        )
        .unwrap()
    }

    async fn built(client: &HttpAgentClient) -> Result<reqwest::Request, AgentError> {
        let req = ChatRequest::new("hi", ConversationId::new("c1").unwrap()).unwrap();
        Ok(client.chat_request(&req).await?.build().unwrap())
    }

    fn header(request: &reqwest::Request, name: &str) -> Option<String> {
        request
            .headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_owned())
    }

    async fn auth_header(client: &HttpAgentClient) -> Option<String> {
        header(&built(client).await.unwrap(), "authorization")
    }

    #[test]
    fn joins_chat_path_onto_base_with_prefix() {
        let client = client("http://agent:8080/api", None);
        assert_eq!(client.chat_url.as_str(), "http://agent:8080/api/v1/chat");
    }

    #[test]
    fn accepts_compose_and_cloud_map_hostnames() {
        for base in [
            "http://agent_runtime:8080",
            "http://runtime.tatarbot.internal:8080",
        ] {
            assert_eq!(
                client(base, None).chat_url.as_str(),
                format!("{base}/v1/chat")
            );
        }
    }

    #[tokio::test]
    async fn sends_bearer_token_only_when_configured() {
        assert_eq!(
            auth_header(&client("http://agent", Some("secret")))
                .await
                .as_deref(),
            Some("Bearer secret")
        );
        assert_eq!(auth_header(&client("http://agent", None)).await, None);
    }

    #[tokio::test]
    async fn sends_id_token_beside_the_api_key_and_rereads_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("id-token");
        std::fs::write(&path, "eyJ.first.sig\n").unwrap();
        let client = client_with_token("https://rt.a.run.app", Some("key"), Some(path.clone()));

        let request = built(&client).await.unwrap();
        assert_eq!(
            header(&request, SERVERLESS_AUTHORIZATION).as_deref(),
            Some("Bearer eyJ.first.sig")
        );
        assert_eq!(
            header(&request, "authorization").as_deref(),
            Some("Bearer key")
        );

        // The host replaces the file (rename); the next request picks the new token up.
        let next = dir.path().join("id-token.new");
        std::fs::write(&next, "eyJ.second.sig").unwrap();
        std::fs::rename(&next, &path).unwrap();
        let request = built(&client).await.unwrap();
        assert_eq!(
            header(&request, SERVERLESS_AUTHORIZATION).as_deref(),
            Some("Bearer eyJ.second.sig")
        );
    }

    #[tokio::test]
    async fn missing_or_empty_id_token_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let missing = client_with_token("https://rt", None, Some(dir.path().join("nope")));
        assert!(matches!(
            built(&missing).await,
            Err(AgentError::Unavailable(_))
        ));

        let empty = dir.path().join("empty");
        std::fs::write(&empty, "  \n").unwrap();
        let empty = client_with_token("https://rt", None, Some(empty));
        assert!(matches!(
            built(&empty).await,
            Err(AgentError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn no_serverless_header_without_a_token_file() {
        let request = built(&client("http://agent", Some("k"))).await.unwrap();
        assert_eq!(header(&request, SERVERLESS_AUTHORIZATION), None);
    }

    #[test]
    fn maps_statuses() {
        assert!(matches!(
            map_status(StatusCode::BAD_REQUEST, r#"{"error":"too long"}"#),
            AgentError::Rejected(m) if m == "too long"
        ));
        assert!(matches!(
            map_status(StatusCode::GATEWAY_TIMEOUT, ""),
            AgentError::Timeout
        ));
        assert!(matches!(
            map_status(StatusCode::SERVICE_UNAVAILABLE, "oops"),
            AgentError::Unavailable(_)
        ));
        assert!(matches!(
            map_status(StatusCode::TOO_MANY_REQUESTS, ""),
            AgentError::Unavailable(_)
        ));
        assert!(matches!(
            map_status(StatusCode::UNAUTHORIZED, r#"{"error":"bad key"}"#),
            AgentError::Unavailable(m) if m == "bad key"
        ));
    }
}
