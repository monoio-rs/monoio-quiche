#![feature(type_alias_impl_trait)]

mod connection;
mod error;
mod h3;

// re-export quiche
pub use connection::Connection;
pub use error::{Error, Result};
pub use h3::H3Connction;
pub use quiche;

mod prelude {
    pub(crate) use quiche::{
        h3::{Config as QuicheH3Config, Connection as QuicheH3Connection},
        Config as QuicheConfig, Connection as QuicheConnection,
    };
    pub(crate) type H3Io = std::rc::Rc<std::cell::RefCell<(bool, Option<std::task::Waker>)>>;
    pub(crate) type StreamIo = std::rc::Rc<
        std::cell::RefCell<std::collections::HashMap<u64, (bool, Option<std::task::Waker>)>>,
    >;
    pub(crate) const MAX_DATAGRAM_SIZE: usize = 1350;
}
