use std::{cell::RefCell, future::Future, rc::Rc, task::Poll};

use monoio::{io::stream::Stream, macros::support::poll_fn, net::udp::UdpSocket};
use quiche::h3::Event;

use crate::{connection::write_io, prelude::*};

pub struct H3Connection {
    io: Rc<UdpSocket>,
    session: Rc<RefCell<QuicheConnection>>,

    h3_io: H3Io,
    write_buffer: Option<Box<[u8; MAX_DATAGRAM_SIZE]>>,

    h3: QuicheH3Connection,
}

impl H3Connection {
    pub fn new_with_transport(
        io: Rc<UdpSocket>,
        session: Rc<RefCell<QuicheConnection>>,
        h3_io: H3Io,
        config: &QuicheH3Config,
    ) -> crate::Result<Self> {
        let h3 = QuicheH3Connection::with_transport(&mut session.borrow_mut(), config)?;
        Ok(Self {
            io,
            session,
            h3_io,
            write_buffer: Some(Box::new([0; MAX_DATAGRAM_SIZE])),
            h3,
        })
    }

    pub async fn send_request<T: quiche::h3::NameValue>(
        &mut self,
        headers: &[T],
        fin: bool,
    ) -> crate::Result<u64> {
        let sid = self
            .h3
            .send_request(&mut self.session.borrow_mut(), headers, fin)?;
        self.write_io().await?;
        Ok(sid)
    }

    pub async fn send_response<T: quiche::h3::NameValue>(
        &mut self,
        stream_id: u64,
        headers: &[T],
        fin: bool,
    ) -> crate::Result<()> {
        self.h3
            .send_response(&mut self.session.borrow_mut(), stream_id, headers, fin)?;
        self.write_io().await?;
        Ok(())
    }

    pub async fn send_body(
        &mut self,
        stream_id: u64,
        body: &[u8],
        fin: bool,
    ) -> crate::Result<usize> {
        let n = self
            .h3
            .send_body(&mut self.session.borrow_mut(), stream_id, body, fin)?;
        self.write_io().await?;
        Ok(n)
    }

    pub fn recv_body(&mut self, stream_id: u64, out: &mut [u8]) -> crate::Result<usize> {
        self.h3
            .recv_body(&mut self.session.borrow_mut(), stream_id, out)
            .map_err(Into::into)
    }

    async fn write_io(&mut self) -> crate::Result<()> {
        let buffer = self.write_buffer.take().unwrap();
        let (res, buffer) = write_io(buffer, &self.session, &self.io).await;
        self.write_buffer = Some(buffer);
        res
    }
}

impl Stream for H3Connection {
    type Item = crate::Result<(u64, Event)>;

    fn next(&mut self) -> impl Future<Output = Option<Self::Item>> {
        async {
            loop {
                // wait h3_io ready
                poll_fn(|cx| {
                    let mut h3_io = self.h3_io.borrow_mut();
                    if h3_io.0 {
                        return Poll::Ready(());
                    }
                    let waker = h3_io.1.get_or_insert_with(|| cx.waker().clone());
                    if !waker.will_wake(cx.waker()) {
                        *waker = cx.waker().clone();
                    }
                    Poll::Pending
                })
                .await;

                let res = self.h3.poll(&mut self.session.borrow_mut());
                match res {
                    Ok(r) => return Some(Ok(r)),
                    Err(quiche::h3::Error::Done) => {
                        if let Err(e) = self.write_io().await {
                            return Some(Err(e));
                        }
                        self.h3_io.borrow_mut().0 = false;
                        continue;
                    }
                    Err(e) => return Some(Err(e.into())),
                }
            }
        }
    }
}
