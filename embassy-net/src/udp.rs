//! UDP sockets.

use core::cell::RefCell;
use core::future::poll_fn;
use core::mem;
use core::task::{Context, Poll};

use embassy_net_driver::Driver;
use smoltcp::iface::{Interface, SocketHandle};
use smoltcp::socket::udp;
pub use smoltcp::socket::udp::PacketMetadata;
use smoltcp::wire::{IpEndpoint, IpListenEndpoint};

use crate::{SocketStack, Stack};

/// Error returned by [`UdpSocket::bind`].
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum BindError {
    /// The socket was already open.
    InvalidState,
    /// No route to host.
    NoRoute,
}

/// Error returned by [`UdpSocket::recv_from`] and [`UdpSocket::send_to`].
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum SendError {
    /// No route to host.
    NoRoute,
    /// Socket not bound to an outgoing port.
    SocketNotBound,
}

/// Error returned by [`UdpSocket::recv_from`] and [`UdpSocket::send_to`].
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum RecvError {
    /// Provided buffer was smaller than the received packet.
    Truncated,
}

/// An UDP socket.
pub struct UdpSocket<'a> {
    stack: &'a RefCell<SocketStack>,
    handle: SocketHandle,
}

impl<'a> UdpSocket<'a> {
    /// Create a new UDP socket using the provided stack and buffers.
    pub fn new<D: Driver>(
        stack: &'a Stack<D>,
        rx_meta: &'a mut [PacketMetadata],
        rx_buffer: &'a mut [u8],
        tx_meta: &'a mut [PacketMetadata],
        tx_buffer: &'a mut [u8],
    ) -> Self {
        let s = &mut *stack.socket.borrow_mut();

        let rx_meta: &'static mut [PacketMetadata] = unsafe { mem::transmute(rx_meta) };
        let rx_buffer: &'static mut [u8] = unsafe { mem::transmute(rx_buffer) };
        let tx_meta: &'static mut [PacketMetadata] = unsafe { mem::transmute(tx_meta) };
        let tx_buffer: &'static mut [u8] = unsafe { mem::transmute(tx_buffer) };
        let handle = s.sockets.add(udp::Socket::new(
            udp::PacketBuffer::new(rx_meta, rx_buffer),
            udp::PacketBuffer::new(tx_meta, tx_buffer),
        ));

        Self {
            stack: &stack.socket,
            handle,
        }
    }

    /// Bind the socket to a local endpoint.
    pub fn bind<T>(&mut self, endpoint: T) -> Result<(), BindError>
    where
        T: Into<IpListenEndpoint>,
    {
        let mut endpoint = endpoint.into();

        if endpoint.port == 0 {
            // If user didn't specify port allocate a dynamic port.
            endpoint.port = self.stack.borrow_mut().get_local_port();
        }

        match self.with_mut(|s, _| s.bind(endpoint)) {
            Ok(()) => Ok(()),
            Err(udp::BindError::InvalidState) => Err(BindError::InvalidState),
            Err(udp::BindError::Unaddressable) => Err(BindError::NoRoute),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&udp::Socket, &Interface) -> R) -> R {
        let s = &*self.stack.borrow();
        let socket = s.sockets.get::<udp::Socket>(self.handle);
        f(socket, &s.iface)
    }

    fn with_mut<R>(&self, f: impl FnOnce(&mut udp::Socket, &mut Interface) -> R) -> R {
        let s = &mut *self.stack.borrow_mut();
        let socket = s.sockets.get_mut::<udp::Socket>(self.handle);
        let res = f(socket, &mut s.iface);
        s.waker.wake();
        res
    }

    /// Receive a datagram.
    ///
    /// This method will wait until a datagram is received.
    ///
    /// Returns the number of bytes received and the remote endpoint.
    pub async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, IpEndpoint), RecvError> {
        poll_fn(move |cx| self.poll_recv_from(buf, cx)).await
    }

    /// Receive a datagram.
    ///
    /// When no datagram is available, this method will return `Poll::Pending` and
    /// register the current task to be notified when a datagram is received.
    ///
    /// When a datagram is received, this method will return `Poll::Ready` with the
    /// number of bytes received and the remote endpoint.
    pub fn poll_recv_from(&self, buf: &mut [u8], cx: &mut Context<'_>) -> Poll<Result<(usize, IpEndpoint), RecvError>> {
        self.with_mut(|s, _| match s.recv_slice(buf) {
            Ok((n, meta)) => Poll::Ready(Ok((n, meta.endpoint))),
            // No data ready
            Err(udp::RecvError::Truncated) => Poll::Ready(Err(RecvError::Truncated)),
            Err(udp::RecvError::Exhausted) => {
                s.register_recv_waker(cx.waker());
                Poll::Pending
            }
        })
    }

    /// Send a datagram to the specified remote endpoint.
    ///
    /// This method will wait until the datagram has been sent.
    ///
    /// When the remote endpoint is not reachable, this method will return `Err(SendError::NoRoute)`
    pub async fn send_to<T>(&self, buf: &[u8], remote_endpoint: T) -> Result<(), SendError>
    where
        T: Into<IpEndpoint>,
    {
        let remote_endpoint: IpEndpoint = remote_endpoint.into();
        poll_fn(move |cx| self.poll_send_to(buf, remote_endpoint, cx)).await
    }

    /// Send a datagram to the specified remote endpoint.
    ///
    /// When the datagram has been sent, this method will return `Poll::Ready(Ok())`.
    ///
    /// When the socket's send buffer is full, this method will return `Poll::Pending`
    /// and register the current task to be notified when the buffer has space available.
    ///
    /// When the remote endpoint is not reachable, this method will return `Poll::Ready(Err(Error::NoRoute))`.
    pub fn poll_send_to<T>(&self, buf: &[u8], remote_endpoint: T, cx: &mut Context<'_>) -> Poll<Result<(), SendError>>
    where
        T: Into<IpEndpoint>,
    {
        self.with_mut(|s, _| match s.send_slice(buf, remote_endpoint) {
            // Entire datagram has been sent
            Ok(()) => Poll::Ready(Ok(())),
            Err(udp::SendError::BufferFull) => {
                s.register_send_waker(cx.waker());
                Poll::Pending
            }
            Err(udp::SendError::Unaddressable) => {
                // If no sender/outgoing port is specified, there is not really "no route"
                if s.endpoint().port == 0 {
                    Poll::Ready(Err(SendError::SocketNotBound))
                } else {
                    Poll::Ready(Err(SendError::NoRoute))
                }
            }
        })
    }

    /// Returns the local endpoint of the socket.
    pub fn endpoint(&self) -> IpListenEndpoint {
        self.with(|s, _| s.endpoint())
    }

    /// Returns whether the socket is open.

    pub fn is_open(&self) -> bool {
        self.with(|s, _| s.is_open())
    }

    /// Close the socket.
    pub fn close(&mut self) {
        self.with_mut(|s, _| s.close())
    }

    /// Returns whether the socket is ready to send data, i.e. it has enough buffer space to hold a packet.
    pub fn may_send(&self) -> bool {
        self.with(|s, _| s.can_send())
    }

    /// Returns whether the socket is ready to receive data, i.e. it has received a packet that's now in the buffer.
    pub fn may_recv(&self) -> bool {
        self.with(|s, _| s.can_recv())
    }

    /// Return the maximum number packets the socket can receive.
    pub fn packet_recv_capacity(&self) -> usize {
        self.with(|s, _| s.packet_recv_capacity())
    }

    /// Return the maximum number packets the socket can receive.
    pub fn packet_send_capacity(&self) -> usize {
        self.with(|s, _| s.packet_send_capacity())
    }

    /// Return the maximum number of bytes inside the recv buffer.
    pub fn payload_recv_capacity(&self) -> usize {
        self.with(|s, _| s.payload_recv_capacity())
    }

    /// Return the maximum number of bytes inside the transmit buffer.
    pub fn payload_send_capacity(&self) -> usize {
        self.with(|s, _| s.payload_send_capacity())
    }

    /// Set the hop limit field in the IP header of sent packets.
    pub fn set_hop_limit(&mut self, hop_limit: Option<u8>) {
        self.with_mut(|s, _| s.set_hop_limit(hop_limit))
    }
}

impl Drop for UdpSocket<'_> {
    fn drop(&mut self) {
        self.stack.borrow_mut().sockets.remove(self.handle);
    }
}

/// UDP stack compatible with `embedded-nal-async` traits.
pub mod nal {
    use core::cell::{Cell, UnsafeCell};
    use core::mem::MaybeUninit;
    use core::ptr::NonNull;

    use embedded_io_async::ErrorKind;
    use embedded_nal_async::{
        ConnectedUdp, ConnectedUdpReceive, ConnectedUdpSend, ConnectedUdpSplit, IpAddr, SocketAddr, SocketAddrV4,
        SocketAddrV6, UdpStack, UnconnectedUdp, UnconnectedUdpReceive, UnconnectedUdpSend, UnconnectedUdpSplit,
    };
    use smoltcp::wire::IpAddress;

    use super::*;

    /// TODO: Doc
    #[derive(Debug)]
    pub enum Error {
        /// TODO: Doc
        Bind(BindError),
        /// TODO: Doc
        Send(SendError),
        /// TODO: Doc
        Recv(RecvError),
    }

    impl From<BindError> for Error {
        fn from(e: BindError) -> Self {
            Self::Bind(e)
        }
    }

    impl From<SendError> for Error {
        fn from(e: SendError) -> Self {
            Self::Send(e)
        }
    }

    impl From<RecvError> for Error {
        fn from(e: RecvError) -> Self {
            Self::Recv(e)
        }
    }

    impl embedded_io_async::Error for Error {
        fn kind(&self) -> ErrorKind {
            ErrorKind::Other // TODO
        }
    }

    /// UDP stack connection pool compatible with `embedded-nal-async` traits.
    ///
    /// The pool is capable of managing up to N concurrent connections with tx and rx buffers according to TX_SZ and RX_SZ.
    pub struct UdpNalStack<'d, D: Driver, const N: usize, const TX_SZ: usize = 1024, const RX_SZ: usize = 1024> {
        stack: &'d Stack<D>,
        state: &'d UdpNalStackState<N, TX_SZ, RX_SZ>,
    }

    impl<'d, D: Driver, const N: usize, const TX_SZ: usize, const RX_SZ: usize> UdpNalStack<'d, D, N, TX_SZ, RX_SZ> {
        /// Create a new `UdpNalStack`.
        pub fn new(stack: &'d Stack<D>, state: &'d UdpNalStackState<N, TX_SZ, RX_SZ>) -> Self {
            Self { stack, state }
        }
    }

    impl<'d, D: Driver, const N: usize, const TX_SZ: usize, const RX_SZ: usize> UdpStack
        for UdpNalStack<'d, D, N, TX_SZ, RX_SZ>
    {
        type Error = Error;

        type Connected<'a> = UdpSocket2<'a, N, TX_SZ, RX_SZ> where Self: 'a;

        type UniquelyBound<'a> = UdpSocket2<'a, N, TX_SZ, RX_SZ> where Self: 'a;

        type MultiplyBound<'a> = UdpSocket2<'a, N, TX_SZ, RX_SZ> where Self: 'a;

        async fn connect_from(
            &self,
            local: SocketAddr,
            remote: SocketAddr,
        ) -> Result<(SocketAddr, Self::Connected<'_>), Self::Error> {
            let mut socket = UdpSocket2::new(self.stack, Some(to_endpoint(&remote)), self.state)?;

            socket.socket.bind(to_endpoint(&local))?;

            Ok((local, socket)) // TODO
        }

        async fn bind_single(&self, local: SocketAddr) -> Result<(SocketAddr, Self::UniquelyBound<'_>), Self::Error> {
            let mut socket = UdpSocket2::new(self.stack, None, self.state)?;

            socket.socket.bind(to_endpoint(&local))?;

            Ok((local, socket)) // TODO
        }

        async fn bind_multiple(&self, local: SocketAddr) -> Result<Self::MultiplyBound<'_>, Self::Error> {
            let mut socket = UdpSocket2::new(self.stack, None, self.state)?;

            socket.socket.bind(to_endpoint(&local))?;

            Ok(socket) // TODO
        }
    }

    /// Opened UDP socket in a [`UdpNalStack`].
    pub struct UdpSocket2<'d, const N: usize, const TX_SZ: usize, const RX_SZ: usize> {
        socket: UdpSocket<'d>,
        remote: Option<IpEndpoint>,
        state: &'d UdpNalStackState<N, TX_SZ, RX_SZ>,
        bufs: NonNull<([u8; TX_SZ], [u8; RX_SZ], [PacketMetadata; 16], [PacketMetadata; 16])>,
    }

    impl<'d, const N: usize, const TX_SZ: usize, const RX_SZ: usize> UdpSocket2<'d, N, TX_SZ, RX_SZ> {
        fn new<D: Driver>(
            stack: &'d Stack<D>,
            remote: Option<IpEndpoint>,
            state: &'d UdpNalStackState<N, TX_SZ, RX_SZ>,
        ) -> Result<Self, Error> {
            let mut bufs = state.pool.alloc().ok_or(BindError::InvalidState)?; // TODO
            Ok(Self {
                socket: unsafe {
                    UdpSocket::new(
                        stack,
                        &mut bufs.as_mut().3,
                        &mut bufs.as_mut().1,
                        &mut bufs.as_mut().2,
                        &mut bufs.as_mut().0,
                    )
                },
                remote,
                state,
                bufs,
            })
        }
    }

    impl<'d, const N: usize, const TX_SZ: usize, const RX_SZ: usize> Drop for UdpSocket2<'d, N, TX_SZ, RX_SZ> {
        fn drop(&mut self) {
            unsafe {
                self.socket.close();
                self.state.pool.free(self.bufs);
            }
        }
    }

    impl<'d, const N: usize, const TX_SZ: usize, const RX_SZ: usize> embedded_io_async::ErrorType
        for UdpSocket2<'d, N, TX_SZ, RX_SZ>
    {
        type Error = Error;
    }

    impl<'d, const N: usize, const TX_SZ: usize, const RX_SZ: usize> embedded_io_async::ErrorType
        for &UdpSocket2<'d, N, TX_SZ, RX_SZ>
    {
        type Error = Error;
    }

    impl<'d, const N: usize, const TX_SZ: usize, const RX_SZ: usize> ConnectedUdpReceive
        for UdpSocket2<'d, N, TX_SZ, RX_SZ>
    {
        async fn receive_into(&mut self, buffer: &mut [u8]) -> Result<usize, Self::Error> {
            loop {
                let (len, remote) = UdpSocket::recv_from(&self.socket, buffer).await?;
                if self.remote.unwrap() == remote {
                    break Ok(len);
                }
            }
        }
    }

    impl<'d, const N: usize, const TX_SZ: usize, const RX_SZ: usize> ConnectedUdpReceive
        for &UdpSocket2<'d, N, TX_SZ, RX_SZ>
    {
        async fn receive_into(&mut self, buffer: &mut [u8]) -> Result<usize, Self::Error> {
            // TODO: Avoid duplication
            loop {
                let (len, remote) = UdpSocket::recv_from(&self.socket, buffer).await?;
                if self.remote.unwrap() == remote {
                    break Ok(len);
                }
            }
        }
    }

    impl<'d, const N: usize, const TX_SZ: usize, const RX_SZ: usize> ConnectedUdpSend for UdpSocket2<'d, N, TX_SZ, RX_SZ> {
        async fn send(&mut self, data: &[u8]) -> Result<(), Self::Error> {
            Ok(UdpSocket::send_to(&self.socket, data, self.remote.unwrap()).await?)
        }
    }

    impl<'d, const N: usize, const TX_SZ: usize, const RX_SZ: usize> ConnectedUdpSend for &UdpSocket2<'d, N, TX_SZ, RX_SZ> {
        async fn send(&mut self, data: &[u8]) -> Result<(), Self::Error> {
            // TODO: Avoid duplication
            Ok(UdpSocket::send_to(&self.socket, data, self.remote.unwrap()).await?)
        }
    }

    impl<'d, const N: usize, const TX_SZ: usize, const RX_SZ: usize> ConnectedUdp for UdpSocket2<'d, N, TX_SZ, RX_SZ> {}

    impl<'d, const N: usize, const TX_SZ: usize, const RX_SZ: usize> ConnectedUdpSplit for UdpSocket2<'d, N, TX_SZ, RX_SZ> {
        type ConnectedReceive<'a> = &'a UdpSocket2<'d, N, TX_SZ, RX_SZ> where Self: 'a;

        type ConnectedSend<'a> = &'a UdpSocket2<'d, N, TX_SZ, RX_SZ> where Self: 'a;

        async fn split(&mut self) -> Result<(Self::ConnectedReceive<'_>, Self::ConnectedSend<'_>), Self::Error> {
            Ok((&*self, &*self))
        }
    }

    impl<'d, const N: usize, const TX_SZ: usize, const RX_SZ: usize> UnconnectedUdpReceive
        for UdpSocket2<'d, N, TX_SZ, RX_SZ>
    {
        async fn receive_into(&mut self, buffer: &mut [u8]) -> Result<(usize, SocketAddr, SocketAddr), Self::Error> {
            let (len, remote) = UdpSocket::recv_from(&self.socket, buffer).await?;

            let local = IpEndpoint {
                addr: self.socket.endpoint().addr.unwrap(), // TODO
                port: self.socket.endpoint().port,
            };

            Ok((len, to_nal_addr(&local), to_nal_addr(&remote)))
        }
    }

    impl<'d, const N: usize, const TX_SZ: usize, const RX_SZ: usize> UnconnectedUdpReceive
        for &UdpSocket2<'d, N, TX_SZ, RX_SZ>
    {
        async fn receive_into(&mut self, buffer: &mut [u8]) -> Result<(usize, SocketAddr, SocketAddr), Self::Error> {
            // TODO: Avoid duplication
            let (len, remote) = UdpSocket::recv_from(&self.socket, buffer).await?;

            let local = IpEndpoint {
                addr: self.socket.endpoint().addr.unwrap(), // TODO
                port: self.socket.endpoint().port,
            };

            Ok((len, to_nal_addr(&local), to_nal_addr(&remote)))
        }
    }

    impl<'d, const N: usize, const TX_SZ: usize, const RX_SZ: usize> UnconnectedUdpSend
        for UdpSocket2<'d, N, TX_SZ, RX_SZ>
    {
        async fn send(
            &mut self,
            _local: SocketAddr, // TODO
            remote: SocketAddr,
            data: &[u8],
        ) -> Result<(), Self::Error> {
            let remote = to_endpoint(&remote);
            Ok(UdpSocket::send_to(&self.socket, data, remote).await?)
        }
    }

    impl<'d, const N: usize, const TX_SZ: usize, const RX_SZ: usize> UnconnectedUdpSend
        for &UdpSocket2<'d, N, TX_SZ, RX_SZ>
    {
        async fn send(
            &mut self,
            _local: SocketAddr, // TODO
            remote: SocketAddr,
            data: &[u8],
        ) -> Result<(), Self::Error> {
            // TODO: Avoid duplication
            let remote = to_endpoint(&remote);
            Ok(UdpSocket::send_to(&self.socket, data, remote).await?)
        }
    }

    impl<'d, const N: usize, const TX_SZ: usize, const RX_SZ: usize> UnconnectedUdp for UdpSocket2<'d, N, TX_SZ, RX_SZ> {}

    impl<'d, const N: usize, const TX_SZ: usize, const RX_SZ: usize> UnconnectedUdpSplit
        for UdpSocket2<'d, N, TX_SZ, RX_SZ>
    {
        type UnconnectedReceive<'a> = &'a UdpSocket2<'d, N, TX_SZ, RX_SZ> where Self: 'a;

        type UnconnectedSend<'a> = &'a UdpSocket2<'d, N, TX_SZ, RX_SZ> where Self: 'a;

        async fn split(&mut self) -> Result<(Self::UnconnectedReceive<'_>, Self::UnconnectedSend<'_>), Self::Error> {
            Ok((&*self, &*self))
        }
    }

    /// State for UdpNalStack
    pub struct UdpNalStackState<const N: usize, const TX_SZ: usize, const RX_SZ: usize> {
        pool: Pool<([u8; TX_SZ], [u8; RX_SZ], [PacketMetadata; 16], [PacketMetadata; 16]), N>,
    }

    impl<const N: usize, const TX_SZ: usize, const RX_SZ: usize> UdpNalStackState<N, TX_SZ, RX_SZ> {
        /// Create a new `UdpNalStackState`.
        pub const fn new() -> Self {
            Self { pool: Pool::new() }
        }
    }

    struct Pool<T, const N: usize> {
        used: [Cell<bool>; N],
        data: [UnsafeCell<MaybeUninit<T>>; N],
    }

    impl<T, const N: usize> Pool<T, N> {
        const VALUE: Cell<bool> = Cell::new(false);
        const UNINIT: UnsafeCell<MaybeUninit<T>> = UnsafeCell::new(MaybeUninit::uninit());

        const fn new() -> Self {
            Self {
                used: [Self::VALUE; N],
                data: [Self::UNINIT; N],
            }
        }
    }

    impl<T, const N: usize> Pool<T, N> {
        fn alloc(&self) -> Option<NonNull<T>> {
            for n in 0..N {
                // this can't race because Pool is not Sync.
                if !self.used[n].get() {
                    self.used[n].set(true);
                    let p = self.data[n].get() as *mut T;
                    return Some(unsafe { NonNull::new_unchecked(p) });
                }
            }
            None
        }

        /// safety: p must be a pointer obtained from self.alloc that hasn't been freed yet.
        unsafe fn free(&self, p: NonNull<T>) {
            let origin = self.data.as_ptr() as *mut T;
            let n = p.as_ptr().offset_from(origin);
            assert!(n >= 0);
            assert!((n as usize) < N);
            self.used[n as usize].set(false);
        }
    }

    fn to_endpoint(addr: &SocketAddr) -> IpEndpoint {
        IpEndpoint {
            addr: match addr.ip() {
                #[cfg(feature = "proto-ipv4")]
                IpAddr::V4(addr) => crate::IpAddress::Ipv4(crate::Ipv4Address::from_bytes(&addr.octets())),
                #[cfg(not(feature = "proto-ipv4"))]
                IpAddr::V4(_) => panic!("ipv4 support not enabled"),
                #[cfg(feature = "proto-ipv6")]
                IpAddr::V6(addr) => crate::IpAddress::Ipv6(crate::Ipv6Address::from_bytes(&addr.octets())),
                #[cfg(not(feature = "proto-ipv6"))]
                IpAddr::V6(_) => panic!("ipv6 support not enabled"),
            },
            port: addr.port(),
        }
    }

    fn to_nal_addr(endpoint: &IpEndpoint) -> SocketAddr {
        match endpoint.addr {
            #[cfg(feature = "proto-ipv4")]
            IpAddress::Ipv4(addr) => SocketAddr::V4(SocketAddrV4::new(addr.0.into(), endpoint.port)),
            #[cfg(feature = "proto-ipv6")]
            IpAddress::Ipv6(addr) => SocketAddr::V6(SocketAddrV6::new(addr.0.into(), endpoint.port, 0, 0)), // TODO
        }
    }
}
