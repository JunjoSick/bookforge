use thiserror::Error;

pub type Result<T> = std::result::Result<T, BookforgeError>;

#[derive(Debug, Error)]
pub enum BookforgeError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("XML error: {0}")]
    Xml(#[from] quick_xml::Error),

    #[error("ZIP error: {0}")]
    Zip(#[from] zip::result::ZipError),
}
