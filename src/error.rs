use std::{io, path::PathBuf};

#[derive(Debug, thiserror::Error)]
pub(crate) enum AppError {
    #[error("cannot read input {path:?}: {source}")]
    Input {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot read standard input: {0}")]
    Stdin(#[source] io::Error),
    #[error("cannot read config {path:?}: {source}")]
    ConfigRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("config {path:?} exceeds the {limit}-byte limit")]
    ConfigTooLarge { path: PathBuf, limit: u64 },
    #[error("invalid config {path:?}: {message}")]
    ConfigParse { path: PathBuf, message: String },
    #[error("cannot encode JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cannot write standard output: {0}")]
    Output(#[source] io::Error),
}

impl AppError {
    pub fn is_stdout_closed(&self) -> bool {
        matches!(self, Self::Output(error) if error.kind() == io::ErrorKind::BrokenPipe)
    }
}

/// Keep untrusted paths and parser messages from issuing terminal commands.
pub(crate) fn diagnostic(error: &impl std::fmt::Display) -> String {
    let mut message = String::new();
    for character in error.to_string().chars() {
        if character.is_control() {
            message.extend(character.escape_default());
        } else {
            message.push(character);
        }
    }
    message
}

#[cfg(test)]
mod tests {
    use super::{AppError, diagnostic};
    use std::io;

    #[test]
    fn only_stdout_broken_pipe_is_a_clean_pipeline_end() {
        assert!(AppError::Output(io::ErrorKind::BrokenPipe.into()).is_stdout_closed());
        assert!(!AppError::Stdin(io::ErrorKind::BrokenPipe.into()).is_stdout_closed());
        assert!(!AppError::Output(io::ErrorKind::PermissionDenied.into()).is_stdout_closed());
    }

    #[test]
    fn diagnostics_escape_terminal_controls() {
        assert_eq!(
            diagnostic(&"bad\u{1b}[2J\npath\t"),
            "bad\\u{1b}[2J\\npath\\t"
        );
    }
}
