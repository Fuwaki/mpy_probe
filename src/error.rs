use std::fmt;

#[derive(Debug)]
pub enum MpError {
    Connection(String),
    Timeout(String),
    Protocol(String),
    Execution { stdout: String, stderr: String },
    Filesystem(String),
    InvalidInput(String),
}

impl fmt::Display for MpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MpError::Connection(msg) => write!(f, "connection error: {}", msg),
            MpError::Timeout(msg) => write!(f, "timeout: {}", msg),
            MpError::Protocol(msg) => write!(f, "protocol error: {}", msg),
            MpError::Execution { stdout, stderr } => {
                write!(f, "execution error")?;
                if !stdout.is_empty() {
                    write!(f, "\nstdout:\n{}", stdout)?;
                }
                if !stderr.is_empty() {
                    write!(f, "\nstderr:\n{}", stderr)?;
                }
                Ok(())
            }
            MpError::Filesystem(msg) => write!(f, "filesystem error: {}", msg),
            MpError::InvalidInput(msg) => write!(f, "invalid input: {}", msg),
        }
    }
}

impl std::error::Error for MpError {}

impl From<std::io::Error> for MpError {
    fn from(e: std::io::Error) -> Self {
        MpError::Connection(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, MpError>;
