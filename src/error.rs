use std::io;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Error, Debug)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("quiche error: {0}")]
    Quiche(#[from] quiche::Error),
    #[error("quiche h3 error: {0}")]
    QuicheH3(#[from] quiche::h3::Error),
}

impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        match e {
            Error::Io(e) => e,
            Error::Quiche(e) => io::Error::new(io::ErrorKind::Other, e),
            Error::QuicheH3(e) => io::Error::new(io::ErrorKind::Other, e),
        }
    }
}
