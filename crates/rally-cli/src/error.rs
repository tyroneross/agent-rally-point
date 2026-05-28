use thiserror::Error;

pub(crate) type Result<T> = std::result::Result<T, RallyError>;

#[derive(Debug, Error)]
pub(crate) enum RallyError {
    #[error("{0}")]
    Usage(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Command(String),
    #[error("{0}")]
    Message(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{context}: {source}")]
    Json {
        context: String,
        #[source]
        source: serde_json::Error,
    },
}

impl RallyError {
    pub(crate) fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) => 2,
            Self::NotFound(_) => 3,
            Self::Command(_) | Self::Message(_) | Self::Io { .. } | Self::Json { .. } => 1,
        }
    }

    pub(crate) fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Self {
        let context = context.into();
        move |source| Self::Io { context, source }
    }

    pub(crate) fn json(context: impl Into<String>) -> impl FnOnce(serde_json::Error) -> Self {
        let context = context.into();
        move |source| Self::Json { context, source }
    }
}
