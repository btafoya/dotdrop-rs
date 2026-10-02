//! Error types shared across dotdrop.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    /// config file parsing / content error
    #[error("{0}")]
    Yaml(String),
    /// config aggregation error
    #[error("{0}")]
    Config(String),
    /// bad command line options
    #[error("{0}")]
    Options(String),
    /// undefined variable / templating error
    #[error("{0}")]
    Undefined(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn yaml_err<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Yaml(msg.into()))
}
