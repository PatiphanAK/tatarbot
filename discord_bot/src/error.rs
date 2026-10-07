/// Process-level errors (startup and wiring). Use-case errors live in
/// `application::ports`.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("missing required environment variable {0}")]
    MissingEnv(&'static str),
    #[error("invalid environment variable {key}: {reason}")]
    InvalidEnv { key: &'static str, reason: String },
    #[error("failed to build agent HTTP client: {0}")]
    AgentClient(#[from] reqwest::Error),
    #[error("discord client error: {0}")]
    Discord(#[source] Box<serenity::Error>),
}

impl From<serenity::Error> for AppError {
    fn from(err: serenity::Error) -> Self {
        AppError::Discord(Box::new(err))
    }
}
