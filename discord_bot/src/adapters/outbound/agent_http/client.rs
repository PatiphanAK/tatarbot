use std::time::Duration;

use async_trait::async_trait;
use reqwest::{StatusCode, Url};

use super::dto::{ChatRequestDto, ChatResponseDto, ErrorDto};
use crate::application::ports::{AgentError, AgentPort};
use crate::domain::{ChatRequest, ChatResponse};

pub struct HttpAgentClient {
    http: reqwest::Client,
    chat_url: Url,
    api_key: Option<String>,
}

impl HttpAgentClient {
    pub fn new(
        base_url: &Url,
        api_key: Option<String>,
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
        })
    }

    fn chat_request(&self, request: &ChatRequest) -> reqwest::RequestBuilder {
        let builder = self
            .http
            .post(self.chat_url.clone())
            .json(&ChatRequestDto::from(request));
        match &self.api_key {
            Some(key) => builder.bearer_auth(key),
            None => builder,
        }
    }
}

#[async_trait]
impl AgentPort for HttpAgentClient {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, AgentError> {
        let response = self
            .chat_request(&request)
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
            tracing::error!(%status, %detail, "agent runtime rejected credentials; check AGENT_API_KEY");
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
        HttpAgentClient::new(
            &Url::parse(base).unwrap(),
            api_key.map(str::to_owned),
            Duration::from_secs(1),
        )
        .unwrap()
    }

    fn auth_header(client: &HttpAgentClient) -> Option<String> {
        let req = ChatRequest::new("hi", ConversationId::new("c1").unwrap()).unwrap();
        let built = client.chat_request(&req).build().unwrap();
        built
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .map(|v| v.to_str().unwrap().to_owned())
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

    #[test]
    fn sends_bearer_token_only_when_configured() {
        assert_eq!(
            auth_header(&client("http://agent", Some("secret"))).as_deref(),
            Some("Bearer secret")
        );
        assert_eq!(auth_header(&client("http://agent", None)), None);
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
