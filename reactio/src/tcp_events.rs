use crate::flat_storage::FlatStorage;
use polling::{Event, Events, PollMode, Poller};
use std::io::Read;
use std::time::Duration;
use std::{io, marker::PhantomData, net::TcpStream};

pub trait TcpStreamHandler {
    fn on_connected(&mut self, sock: &mut std::net::TcpStream) -> bool;
    /// \return false to close socket.
    fn on_readable(&mut self, sock: &mut std::net::TcpStream) -> bool;

    // fn on_remove(self, conn: &std::net::TcpStream);
}

pub trait TcpListenerHandler {
    /// \return null to close new connection.
    fn on_new_connection(
        &mut self,
        sock: &mut std::net::TcpListener,
        new_sock: &mut std::net::TcpStream,
        addr: std::net::SocketAddr,
    ) -> Option<Box<dyn TcpStreamHandler>>;

    // fn on_remove(self, conn: &std::net::TcpListener);
}

/// TcpPollMgr manages TCP connections.
pub struct TcpPollMgr {
    connections: TcpConnections,
    events: Events,
}

// Make a seperate struct TcpConnections because when interating TcpConnectionMgr::events, sessions must be mutable in process_events.
struct TcpConnections {
    socket_handlers: FlatStorage<TcpSocketHandler>,
    poller: Poller,
    count_streams: usize, // TcpStreams only (excluding TcpListener)
}

enum TcpSocketHandler {
    ListenerType(std::net::TcpListener, Box<dyn TcpListenerHandler>), // <sock, handler, key_in_flat_storage>
    StreamType(std::net::TcpStream, Box<dyn TcpStreamHandler>),
}

pub struct SocketKey(usize); // identify a socket in TcpConnections.
pub const INVALID_SOCKET_KEY: SocketKey = SocketKey(usize::MAX);

impl TcpConnections {
    pub fn new() -> Self {
        Self {
            socket_handlers: FlatStorage::new(),
            poller: Poller::new().unwrap(),
            count_streams: 0,
        }
    }
    /// \return number of all managed socks.
    pub fn len(&self) -> usize {
        self.socket_handlers.len()
    }
    pub fn add_stream(
        &mut self,
        sock: std::net::TcpStream,
        handler: Box<dyn TcpStreamHandler>,
    ) -> SocketKey {
        let key = self
            .socket_handlers
            .add(TcpSocketHandler::StreamType(sock, handler));
        if let TcpSocketHandler::StreamType(ref sock, _) = self.socket_handlers.get(key).unwrap() {
            unsafe {
                self.poller
                    .add_with_mode(sock, Event::readable(key), PollMode::Level)
                    .unwrap();
            }
        }
        self.count_streams += 1;
        SocketKey(key)
    }

    pub fn add_listener(
        &mut self,
        sock: std::net::TcpListener,
        handler: Box<dyn TcpListenerHandler>,
    ) -> SocketKey {
        let key = self
            .socket_handlers
            .add(TcpSocketHandler::ListenerType(sock, handler));
        if let TcpSocketHandler::ListenerType(ref sock, _) = self.socket_handlers.get(key).unwrap()
        {
            // must read exaustively.
            unsafe {
                self.poller
                    .add_with_mode(sock, Event::readable(key), PollMode::Level)
                    .unwrap();
            }
        }
        SocketKey(key)
    }

    pub fn remove_by_key(&mut self, key: SocketKey) {
        if let Some(sockhandler) = self.socket_handlers.get_mut(key.0) {
            match *sockhandler {
                TcpSocketHandler::StreamType(ref mut sock, _) => {
                    println!("removing key: {}, sock: {:?}", key.0, sock);
                    self.count_streams -= 1;
                    self.poller.delete(sock).unwrap();
                }
                TcpSocketHandler::ListenerType(ref mut sock, _) => {
                    println!("removing key: {}, sock: {:?}", key.0, sock);
                    self.poller.delete(sock).unwrap();
                }
            }
        }
        self.socket_handlers.remove(key.0); // sock is auto closed when being dropped.
    }

    /// \local_addr  ip:port. e.g. "127.0.0.1:8000"
    pub fn start_listen(
        &mut self,
        local_addr: &str,
        handler: Box<dyn TcpListenerHandler>,
    ) -> std::io::Result<SocketKey> {
        let socket = std::net::TcpListener::bind(local_addr)?;
        socket.set_nonblocking(true)?;
        Result::Ok(self.add_listener(socket, handler))
    }

    pub fn start_connect(
        &mut self,
        remote_addr: &str,
        handler: Box<dyn TcpStreamHandler>,
    ) -> std::io::Result<SocketKey> {
        let mut socket = TcpStream::connect(remote_addr)?;
        let mut handler = handler;
        socket.set_nonblocking(true)?; // FIXME: use blocking socket and each nonblocking read and blocking write.
        if handler.on_connected(&mut socket) {
            return Result::Ok(self.add_stream(socket, handler));
        }
        Result::Ok(INVALID_SOCKET_KEY)
    }
}

impl Default for TcpPollMgr {
    fn default() -> Self {
        Self::new()
    }
}
impl TcpPollMgr {
    pub fn new() -> Self {
        Self {
            connections: TcpConnections::new(),
            events: Events::new(),
        }
    }
    /// \return all socks (listener & stream)
    pub fn len(&self) -> usize {
        self.connections.len()
    }
    pub fn count_streams(&self) -> usize {
        self.connections.count_streams
    }
    pub fn start_listen(
        &mut self,
        local_addr: &str,
        handler: Box<dyn TcpListenerHandler>,
    ) -> std::io::Result<SocketKey> {
        self.connections.start_listen(local_addr, handler)
    }

    pub fn start_connect(
        &mut self,
        remote_addr: &str,
        handler: Box<dyn TcpStreamHandler>,
    ) -> std::io::Result<SocketKey> {
        self.connections.start_connect(remote_addr, handler)
    }

    /// return true to indicate some events has been processed.
    pub fn process_events(&mut self) -> bool {
        self.events.clear();
        self.connections
            .poller
            .wait(&mut self.events, Some(Duration::from_secs(0)))
            .unwrap(); // None duration means forever

        for ev in self.events.iter() {
            let mut removesock = false;
            if ev.readable {
                if let Some(sockhandler) = self.connections.socket_handlers.get_mut(ev.key) {
                    match sockhandler {
                        TcpSocketHandler::ListenerType(ref mut sock, ref mut handler) => {
                            let (mut newsock, addr) = sock.accept().unwrap();
                            if let Some(mut newhandler) =
                                handler.on_new_connection(sock, &mut newsock, addr)
                            {
                                newsock.set_nonblocking(true).unwrap();
                                if newhandler.on_connected(&mut newsock) {
                                    self.connections.add_stream(newsock, newhandler);
                                }
                            }
                            // else newsock will auto destroy
                        }
                        TcpSocketHandler::StreamType(ref mut sock, ref mut handler) => {
                            removesock = !handler.on_readable(sock);
                        }
                    }
                } else {
                    panic!("no event key!");
                }
            }
            if ev.is_err().unwrap_or(false) {
                println!("socket error key: {}", ev.key);
                removesock = true;
            }
            if ev.is_interrupt() {
                println!("socket interrupt key: {}", ev.key);
                removesock = true;
            }
            if removesock {
                self.connections.remove_by_key(SocketKey(ev.key));
            }
        }

        return !self.events.is_empty();
    }
}

//====================================================================================
//         Default TcpListenerHandler
//====================================================================================
pub struct DefaultTcpListenerHandler<StreamHandler> {
    _phantom: PhantomData<StreamHandler>,
}

impl<StreamHandler: TcpStreamHandler + Default + 'static> DefaultTcpListenerHandler<StreamHandler> {
    pub fn new_boxed() -> Box<Self> {
        Box::new(Self::default())
    }
}
impl<StreamHandler: TcpStreamHandler + Default + 'static> Default
    for DefaultTcpListenerHandler<StreamHandler>
{
    fn default() -> Self {
        Self {
            _phantom: PhantomData::default(),
        }
    }
}
impl<StreamHandler: TcpStreamHandler + Default + 'static> TcpListenerHandler
    for DefaultTcpListenerHandler<StreamHandler>
{
    fn on_new_connection(
        &mut self,
        _conn: &mut std::net::TcpListener,
        _new_conn: &mut std::net::TcpStream,
        _addr: std::net::SocketAddr,
    ) -> Option<Box<dyn TcpStreamHandler>> {
        Some(Box::new(StreamHandler::default()))
    }
}

#[cfg(test)]
mod test {
    use io::{ErrorKind, Write};

    use super::*;

    struct TcpEchoHandler {
        recv_buf: Vec<u8>,
        is_client: bool, // default false
        max_echo: i32,
        count_echo: i32,
    }
    impl Default for TcpEchoHandler {
        fn default() -> Self {
            Self {
                recv_buf : vec![0u8; 1024],
                is_client: false,
                max_echo: i32::MAX,
                count_echo: 0,
            }
        }
    }
    impl TcpEchoHandler {
        fn new_client(a_max_echo: i32) -> Box<Self> {
            Box::new(Self {
                recv_buf : vec![0u8; 1024],
                is_client: true,
                max_echo: a_max_echo,
                count_echo: 0,
            })
        }
    }
    impl TcpStreamHandler for TcpEchoHandler {
        fn on_connected(&mut self, sock: &mut std::net::TcpStream) -> bool {
            if self.is_client {
                println!("client sock: {:?} connected.", sock);
                sock.write(b"test msg00").unwrap();
            } else {
                println!("server sock: {sock:?} connected.");
            }
            return true;
        }
        fn on_readable(&mut self, sock: &mut std::net::TcpStream) -> bool {
            loop {
                match sock.read(&mut self.recv_buf) {
                    io::Result::Ok(nbytes) => {
                        if nbytes == 0 {
                            println!("peer closed sock: {:?}", sock);
                            return false;
                        }
                        if self.count_echo < self.max_echo {
                            println!("sock: {:?} recv bytes: {}", sock, nbytes);
                            sock.write(&self.recv_buf[..nbytes]).unwrap();
                            self.count_echo += 1;
                            if nbytes < self.recv_buf.len() {
                                return true; // no more data
                            }
                            continue; // try next read until error or WouldBlock
                        } else {
                            println!(
                                "sock:{:?} recv bytes: {}, reached target. closing.",
                                sock, nbytes
                            );
                            return false;
                        }
                    }
                    io::Result::Err(err) => {
                        let errkind = err.kind();
                        if errkind == ErrorKind::WouldBlock {
                            // println!("WouldBlock {sock:?}");
                            return true;
                        }
                        println!("ERROR recv sock: {sock:?} error: {}, errkind: {errkind:?}", err);
                        return false;
                    }
                }
            }
        }
    }
    #[test]
    pub fn test_tcp_event_handler() {
        let addr = "127.0.0.1:12355";
        let mut mgr = TcpPollMgr::new();
        mgr.start_listen(
            addr,
            DefaultTcpListenerHandler::<TcpEchoHandler>::new_boxed(),
        )
        .unwrap();
        mgr.start_connect(addr, TcpEchoHandler::new_client(2))
            .unwrap();
        while mgr.count_streams() > 0 {
            mgr.process_events();
        }
        assert_eq!(mgr.len(), 1); // remaining listener
    }
}
