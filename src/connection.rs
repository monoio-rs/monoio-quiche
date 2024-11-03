use std::{
    cell::RefCell,
    collections::HashMap,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs},
    rc::Rc,
    task::Poll,
};

use local_sync::oneshot;
use monoio::{buf::IoBuf, io::Canceller, macros::support::poll_fn, net::udp::UdpSocket};
use quiche::ConnectionId;

use crate::{prelude::*, H3Connection};

pub struct Connection {
    io: Rc<UdpSocket>,
    session: Rc<RefCell<QuicheConnection>>,

    _read_task: oneshot::Receiver<()>,
    write_buffer: Option<Box<[u8; MAX_DATAGRAM_SIZE]>>,

    stream_io: StreamIo,
    h3_io: H3Io,
}

impl Connection {
    pub async fn connect<'a, A: ToSocketAddrs>(
        addr: A,
        server_name: Option<&str>,
        scid: &ConnectionId<'a>,
        config: &mut QuicheConfig,
    ) -> crate::Result<Self> {
        let peer_addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "empty address"))?;
        let bind_addr = match peer_addr {
            SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
        };

        Self::connect_with_addr(server_name, scid, bind_addr, peer_addr, config).await
    }

    pub async fn connect_with_addr<'a>(
        server_name: Option<&str>,
        scid: &ConnectionId<'a>,
        local: SocketAddr,
        peer: SocketAddr,
        config: &mut QuicheConfig,
    ) -> crate::Result<Self> {
        let socket = Rc::new(UdpSocket::bind(local)?);
        let local_addr = socket.local_addr()?;
        let mut canceller = Canceller::new();
        let mut conn = quiche::connect(server_name, scid, local_addr, peer, config)?;

        // start handshake
        let mut buffer = Box::new([0; MAX_DATAGRAM_SIZE]);
        loop {
            // send until there's nothing to send
            match conn.send(&mut buffer[..]) {
                Ok((n, send_info)) => {
                    let send_slice = buffer.slice(..n);
                    let (res, buf) = socket.send_to(send_slice, send_info.to).await;
                    buffer = buf.into_inner();
                    res?;
                    continue;
                }
                Err(quiche::Error::Done) => {
                    // there's nothing to send
                }
                Err(e) => {
                    return Err(e.into());
                }
            }

            // check state
            if conn.is_closed() {
                return Err(crate::Error::Quiche(quiche::Error::InvalidState));
            }
            if conn.is_established() {
                let session = Rc::new(RefCell::new(conn));
                let stream_io = Rc::new(RefCell::new(HashMap::new()));
                let h3_io = Rc::new(RefCell::new((true, None)));
                let read_task = Self::start_read_task(
                    local_addr,
                    socket.clone(),
                    session.clone(),
                    stream_io.clone(),
                    h3_io.clone(),
                );
                return Ok(Self {
                    io: socket,
                    session,
                    _read_task: read_task,
                    write_buffer: Some(buffer),
                    stream_io,
                    h3_io,
                });
            }

            // recv with timeout
            let mut len_from = None;
            if let Some(timeout) = conn.timeout() {
                let recv = socket.cancelable_recv_from(buffer, canceller.handle());
                let sleep = monoio::time::sleep(timeout);
                crate::pin!(recv);
                crate::pin!(sleep);
                monoio::select! {
                    _ = &mut sleep => {
                        canceller = canceller.cancel();
                        let (res, buf) = recv.await;
                        buffer = buf;
                        match res {
                            Ok(r) => len_from = Some(r),
                            Err(_) => {
                                conn.on_timeout();
                            }
                        }
                    },
                    (res, buf) = &mut recv => {
                        buffer = buf;
                        len_from = Some(res?);
                    }
                }
            } else {
                let (res, buf) = socket.recv_from(buffer).await;
                buffer = buf;
                len_from = Some(res?);
            }
            if let Some((len, from)) = len_from {
                let recv_info = quiche::RecvInfo {
                    to: local_addr,
                    from,
                };
                conn.recv(&mut buffer[..len], recv_info)?;
            }

            // check state
            if conn.is_closed() {
                return Err(crate::Error::Quiche(quiche::Error::InvalidState));
            }
        }
    }

    pub fn h3(&self, config: &QuicheH3Config) -> crate::Result<H3Connection> {
        H3Connection::new_with_transport(
            self.io.clone(),
            self.session.clone(),
            self.h3_io.clone(),
            config,
        )
    }

    pub fn io(&self) -> Rc<UdpSocket> {
        self.io.clone()
    }

    pub fn session(&self) -> Rc<RefCell<QuicheConnection>> {
        self.session.clone()
    }

    pub async fn stream_send(
        &mut self,
        stream_id: u64,
        buf: &[u8],
        fin: bool,
    ) -> crate::Result<usize> {
        let n = loop {
            let res = self.session.borrow_mut().stream_send(stream_id, buf, fin);
            match res {
                Ok(n) => break n,
                Err(quiche::Error::Done) => {
                    // no space left, we have to flush data out
                    self.write_io().await?;
                }
                Err(e) => return Err(e.into()),
            }
        };
        self.write_io().await?;
        Ok(n)
    }

    pub async fn stream_recv(
        &mut self,
        stream_id: u64,
        out: &mut [u8],
    ) -> crate::Result<(usize, bool)> {
        loop {
            let res = self.session.borrow_mut().stream_recv(stream_id, out);
            match res {
                Ok((n, fin)) => {
                    return Ok((n, fin));
                }
                Err(quiche::Error::Done) => {
                    // wait for ready
                    tracing::debug!("waiting for stream {stream_id}");
                    self.stream_io.borrow_mut().entry(stream_id).or_default();

                    poll_fn(|cx| {
                        let mut sr = self.stream_io.borrow_mut();
                        let (ready, maybe_waker) = sr.get_mut(&stream_id).unwrap();
                        if *ready {
                            return Poll::Ready(());
                        }
                        let waker = maybe_waker.get_or_insert_with(|| cx.waker().clone());
                        if !waker.will_wake(cx.waker()) {
                            *waker = cx.waker().clone();
                        }
                        Poll::Pending
                    })
                    .await;
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
        }
    }

    pub async fn close(&mut self, app: bool, err: u64, reason: &[u8]) -> crate::Result<()> {
        self.session.borrow_mut().close(app, err, reason)?;
        let _ = self.write_io().await;
        Ok(())
    }

    fn start_read_task(
        local_addr: SocketAddr,
        socket: Rc<UdpSocket>,
        session: Rc<RefCell<QuicheConnection>>,
        stream_reader: StreamIo,
        h3_io: H3Io,
    ) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        monoio::spawn(async move {
            let res = Self::read_loop(tx, local_addr, socket, session, stream_reader, h3_io).await;
            tracing::info!("read task exit {res:?}");
        });
        rx
    }

    // TODO: what if some error happens? we should sense read task exit
    async fn read_loop(
        mut notified: oneshot::Sender<()>,
        local_addr: SocketAddr,
        socket: Rc<UdpSocket>,
        session: Rc<RefCell<QuicheConnection>>,
        stream_io: StreamIo,
        h3_io: H3Io,
    ) -> crate::Result<()> {
        fn wakeup(stream_io: &StreamIo, session: &Rc<RefCell<QuicheConnection>>, h3_io: &H3Io) {
            let sess = session.borrow_mut();

            // wake stream readers
            let mut stream_io = stream_io.borrow_mut();
            if !stream_io.is_empty() {
                for stream in sess.readable() {
                    let (ready, waker) = stream_io.entry(stream).or_default();
                    *ready = true;
                    if let Some(waker) = waker.take() {
                        tracing::debug!("wake stream {stream}");
                        waker.wake();
                    }
                }
            }

            // wake h3 waker
            let mut h3_io = h3_io.borrow_mut();
            h3_io.0 = true;
            if let Some(waker) = h3_io.1.take() {
                tracing::debug!("wake h3");
                waker.wake();
            }
        }

        let mut buffer = Box::new([0; MAX_DATAGRAM_SIZE]);
        let mut canceller = Canceller::new();
        let exit = notified.closed();
        crate::pin!(exit);

        loop {
            let timeout = session.borrow().timeout();
            let (len, from) = if let Some(timeout) = timeout {
                let recv = socket.cancelable_recv_from(buffer, canceller.handle());
                let sleep = monoio::time::sleep(timeout);
                crate::pin!(recv);
                crate::pin!(sleep);
                monoio::select! {
                    (res, buf) = &mut recv => {
                        buffer = buf;
                        res?
                    },
                    _ = &mut sleep => {
                        canceller = canceller.cancel();
                        let (res, buf) = recv.await;
                        buffer = buf;
                        match res {
                            Ok(r) => r,
                            Err(_) => {
                                let mut s = session.borrow_mut();
                                s.on_timeout();
                                if s.is_closed() {
                                    return Ok(());
                                }
                                continue;
                            }
                        }
                    },
                    _ = &mut exit => {
                        return Ok(());
                    }
                }
            } else {
                let (res, buf) = socket.recv_from(buffer).await;
                buffer = buf;
                res?
            };

            let recv_info = quiche::RecvInfo {
                to: local_addr,
                from,
            };
            session.borrow_mut().recv(&mut buffer[..len], recv_info)?;
            wakeup(&stream_io, &session, &h3_io);
        }
    }

    /// flush from session to socket, if there is nothing to send, return.
    pub async fn write_io(&mut self) -> crate::Result<()> {
        let buffer = self.write_buffer.take().unwrap();
        let (res, buffer) = write_io(buffer, &self.session, &self.io).await;
        self.write_buffer = Some(buffer);
        res
    }
}

pub(crate) async fn write_io<const N: usize>(
    mut buffer: Box<[u8; N]>,
    session: &Rc<RefCell<QuicheConnection>>,
    io: &Rc<UdpSocket>,
) -> (crate::Result<()>, Box<[u8; N]>) {
    loop {
        let res = session.borrow_mut().send(&mut buffer[..]);
        match res {
            Ok((n, send_info)) => {
                let send_slice = buffer.slice(..n);
                let (res, buf) = io.send_to(send_slice, send_info.to).await;
                buffer = buf.into_inner();
                match res {
                    Ok(_) => continue,
                    Err(e) => {
                        return (Err(e.into()), buffer);
                    }
                }
            }
            Err(quiche::Error::Done) => {
                return (Ok(()), buffer);
            }
            Err(e) => {
                return (Err(e.into()), buffer);
            }
        }
    }
}
