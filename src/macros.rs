#[macro_export]
macro_rules! pin {
    ($($x:ident),*) => {$(
        let mut $x = ::std::pin::pin!($x);
    )*};
}