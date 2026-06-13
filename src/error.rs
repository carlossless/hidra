//! Error types for hidra.

use core::fmt;

/// Convenience alias used by every fallible API in this crate.
pub type HidResult<T> = Result<T, HidError>;

/// Errors returned by hidra.
///
/// This maps the information `hid_error()` exposes in hidapi onto a typed
/// Rust enum, so callers can match on the failure class instead of parsing
/// strings.
#[derive(Debug)]
#[non_exhaustive]
pub enum HidError {
    /// The platform backend failed to initialize (`hid_init` equivalent).
    Initialization { message: String },
    /// No device matched the requested VID/PID/serial or path.
    DeviceNotFound,
    /// The device exists but could not be opened (permissions, exclusive
    /// access by another process, ...).
    OpenFailed { message: String },
    /// An operating-system level I/O error.
    #[cfg(not(target_arch = "wasm32"))]
    Io {
        operation: &'static str,
        source: std::io::Error,
    },
    /// The device was disconnected while in use.
    Disconnected,
    /// Data passed to a send/write call is invalid (e.g. empty report).
    InvalidData { message: String },
    /// A report descriptor (or other HID structure) failed to parse.
    Parse { message: String },
    /// The operation is not supported by this backend.
    Unsupported { message: String },
    /// A backend-specific failure that fits no other category
    /// (the catch-all hidapi reports through `hid_error`).
    Backend { message: String },
}

impl HidError {
    /// Shorthand used by backends for `Backend` errors.
    pub(crate) fn backend(message: impl Into<String>) -> Self {
        HidError::Backend {
            message: message.into(),
        }
    }
}

impl fmt::Display for HidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HidError::Initialization { message } => {
                write!(f, "failed to initialize HID backend: {message}")
            }
            HidError::DeviceNotFound => write!(f, "device not found"),
            HidError::OpenFailed { message } => write!(f, "failed to open device: {message}"),
            #[cfg(not(target_arch = "wasm32"))]
            HidError::Io { operation, source } => write!(f, "{operation}: {source}"),
            HidError::Disconnected => write!(f, "device disconnected"),
            HidError::InvalidData { message } => write!(f, "invalid data: {message}"),
            HidError::Parse { message } => write!(f, "parse error: {message}"),
            HidError::Unsupported { message } => write!(f, "unsupported operation: {message}"),
            HidError::Backend { message } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for HidError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            HidError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl HidError {
    pub(crate) fn io(operation: &'static str, source: std::io::Error) -> Self {
        HidError::Io { operation, source }
    }

    // Not every backend needs this helper.
    #[allow(dead_code)]
    pub(crate) fn last_os_error(operation: &'static str) -> Self {
        HidError::Io {
            operation,
            source: std::io::Error::last_os_error(),
        }
    }
}
