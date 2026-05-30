//! Journal-canonical input mode (docs/journal-canonical-input.md).
//!
//! When `HIVE_INPUT_MODE=enforce`, structured rows (tasks, notes, links, …)
//! are read-only over HTTP; only `POST /journal` and the wire surfaces accept
//! new facts. Shadow mode logs would-be blocks without rejecting.

use crate::error::ApiError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMode {
    #[default]
    Legacy,
    Shadow,
    Enforce,
}

impl InputMode {
    pub fn from_env() -> Self {
        match std::env::var("HIVE_INPUT_MODE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "shadow" => Self::Shadow,
            "enforce" => Self::Enforce,
            _ => Self::Legacy,
        }
    }
}

/// Guard direct writes to structured tables. Call at the top of POST/PATCH/DELETE
/// handlers that bypass journal projection.
pub fn guard_structured_write(mode: InputMode, surface: &str) -> Result<(), ApiError> {
    match mode {
        InputMode::Legacy => Ok(()),
        InputMode::Shadow => {
            tracing::warn!(
                surface,
                "structured write would be blocked under journal-only input (shadow; allowing)"
            );
            Ok(())
        }
        InputMode::Enforce => Err(ApiError::Forbidden(format!(
            "direct structured write blocked ({surface}); use POST /journal — tasks, notes, and links project from journal prose (see docs/journal-canonical-input.md)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_allows_structured_writes() {
        guard_structured_write(InputMode::Legacy, "POST /tasks").unwrap();
    }

    #[test]
    fn enforce_blocks_structured_writes() {
        let err = guard_structured_write(InputMode::Enforce, "POST /tasks").unwrap_err();
        assert!(err.to_string().contains("POST /journal"));
    }
}
