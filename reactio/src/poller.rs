use crate::dbglog; // import one macro per line.
use crate::flat_storage::FlatStorage;
use crate::logmsg;
use crate::utils;
use polling::{Event, Events, PollMode, Poller};
use std::time::Duration;
use std::{marker::PhantomData, net::TcpStream};

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

/// SocketPoller manages TCP connections & polling.
pub struct SocketPoller {
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
                    logmsg!("removing key: {}, sock: {:?}", key.0, sock);
                    self.count_streams -= 1;
                    self.poller.delete(sock).unwrap();
                }
                TcpSocketHandler::ListenerType(ref mut sock, _) => {
                    logmsg!("removing key: {}, sock: {:?}", key.0, sock);
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

impl Default for SocketPoller {
    fn default() -> Self {
        Self::new()
    }
}
impl SocketPoller {
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
                logmsg!("WARN: socket error key: {}", ev.key);
                removesock = true;
            }
            if ev.is_interrupt() {
                logmsg!("WARN: socket interrupt key: {}", ev.key);
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

// enum IOResult {
//     Ok(usize), // bytes > 0
//     WouldBlock,
//     Closed,  // peer closed
//     Error(std::io::Error),
// }
// /// read all remaining bytes from unblocking socket.
// /// \note sock must be set nonblocking before calling this function.
// pub fn tcp_read(sock: &std::net::TcpStream, buf: &mut [u8]) -> IOResult {
//     match sock.read(&mut buf) {
//         io::Result::Ok(nbytes) => {
//             if nbytes == 0 {
//                 println!("peer closed sock: {:?}", sock);
//                 return IOResult::Closed;
//             }
//             if nbytes < self.recv_buf.len() {
//                 return IOResult::Ok(nbytes); // no more data
//             }
//         },
//         io::Result::Err(err) => {
//             let errkind = err.kind();
//             if errkind == ErrorKind::WouldBlock {
//                 return IOResult::WouldBlock;
//             } else if errkind == ErrorKind::ConnectionReset {
//                 return IOResult::Closed;
//             }
//             return IOResult::Error(err);
//         }
//     }
// }

pub mod sample {
    use std::io::{ErrorKind, Read, Write};

    use super::*;

    #[derive(Copy, Clone)]
    pub struct MsgHeader {
        body_len: u16,
        send_time: i64, // sending timestamp nanos since epoch
    }
    const MSG_HEADER_SIZE: usize = 10;
    const RECV_BUFFER_SIZE: usize = 1024;
    const LATENCY_BATCH_SIZE: i32 = 10000;

    pub struct TcpEchoHandler {
        recv_buf: Vec<u8>,
        is_client: bool, // default false
        max_echo: i32,
        count_echo: i32,

        latency_batch: i32, // number of messages to report latencies.
        last_sent_time: i64,
        single_trip_durations: Vec<i64>,
        round_trip_durations: Vec<i64>,
    }
    impl Default for TcpEchoHandler {
        fn default() -> Self {
            TcpEchoHandler::new(false, i32::MAX, LATENCY_BATCH_SIZE)
        }
    }
    impl TcpEchoHandler {
        pub fn new(isclient: bool, a_max_echo: i32, a_latency_batch: i32) -> Self {
            Self {
                recv_buf: vec![0u8; RECV_BUFFER_SIZE],
                is_client: isclient,
                max_echo: a_max_echo,
                count_echo: 0,
                latency_batch: a_latency_batch,
                //
                last_sent_time: 0,
                single_trip_durations: Vec::new(),
                round_trip_durations: Vec::new(),
            }
        }

        pub fn new_client(max_echo: i32, latency_batch: i32) -> Box<Self> {
            Box::new(Self::new(true, max_echo, latency_batch))
        }

        fn report_latencies(&mut self) {
            println!(
                "Latencies(us) size: {} min      50%       99%     max",
                self.round_trip_durations.len()
            );
            self.round_trip_durations.sort();
            self.single_trip_durations.sort();
            let (d, n) = (
                &self.single_trip_durations[..],
                self.single_trip_durations.len(),
            );
            println!(
                "SingleTrip        {}       {}      {}      {}",
                d[0] / 1000,
                d[n / 2] / 1000,
                d[(n as f32 * 0.99) as usize] / 1000,
                d[n - 2] / 1000
            );

            let (d, n) = (
                &self.round_trip_durations[..],
                self.round_trip_durations.len(),
            );
            println!(
                "RoundTrip         {}       {}      {}      {}",
                d[0] / 1000,
                d[n / 2] / 1000,
                d[(n as f32 * 0.99) as usize] / 1000,
                d[n - 2] / 1000
            );
            self.single_trip_durations.clear();
            self.round_trip_durations.clear();
        }
    }
    impl TcpStreamHandler for TcpEchoHandler {
        fn on_connected(&mut self, sock: &mut std::net::TcpStream) -> bool {
            if self.is_client {
                logmsg!("client sock: {:?} connected.", sock);
                let msg_content = b"test msg00";
                debug_assert!(msg_content.len() + MSG_HEADER_SIZE < RECV_BUFFER_SIZE);

                self.recv_buf[10..(10 + msg_content.len())].copy_from_slice(&msg_content[..]);
                let header: &mut MsgHeader = utils::bytes_to_ref_mut(&mut self.recv_buf[0..10]);
                header.body_len = msg_content.len() as u16;
                header.send_time = utils::now_nanos();

                //
                let nsent = sock
                    .write(&self.recv_buf[..(msg_content.len() + MSG_HEADER_SIZE)])
                    .unwrap();
                debug_assert_eq!(nsent, msg_content.len() + MSG_HEADER_SIZE);
                dbglog!("client sent initial msg");
            } else {
                logmsg!("server sock: {sock:?} connected.");
            }
            return true;
        }
        fn on_readable(&mut self, sock: &mut std::net::TcpStream) -> bool {
            loop {
                match sock.read(&mut self.recv_buf) {
                    std::io::Result::Ok(nbytes) => {
                        if nbytes == 0 {
                            logmsg!("peer closed sock: {:?}", sock);
                            return false;
                        }
                        let recvtime = utils::now_nanos();
                        if self.count_echo < self.max_echo {
                            dbglog!("sock: {:?} recv bytes: {}, will echo back.", sock, nbytes);
                            // decode message header
                            debug_assert!(nbytes >= MSG_HEADER_SIZE);
                            {
                                let header: &mut MsgHeader =
                                    utils::bytes_to_ref_mut(&mut self.recv_buf[0..10]);
                                debug_assert_eq!(
                                    header.body_len as usize,
                                    nbytes - MSG_HEADER_SIZE
                                );

                                if self.last_sent_time > 0 {
                                    self.round_trip_durations
                                        .push(recvtime - self.last_sent_time);
                                    self.single_trip_durations.push(recvtime - header.send_time);
                                }
                            }
                            // here drop header because report_latencies is one more but borrow.
                            if self.round_trip_durations.len() as i32 == self.latency_batch {
                                self.report_latencies();
                            }
                            let header: &mut MsgHeader =
                                utils::bytes_to_ref_mut(&mut self.recv_buf[0..10]);
                            header.send_time = utils::now_nanos(); // update send_time only

                            let nsent = sock.write(&self.recv_buf[..nbytes]).unwrap();
                            debug_assert_eq!(nsent, nbytes);
                            self.last_sent_time = utils::now_nanos();

                            self.count_echo += 1;
                            if nbytes < self.recv_buf.len() {
                                return true; // no more data
                            }
                            continue; // try next read until error or WouldBlock
                        } else {
                            logmsg!(
                                "sock:{:?} recv bytes: {}, reached target. closing.",
                                sock,
                                nbytes
                            );
                            return false;
                        }
                    }
                    std::io::Result::Err(err) => {
                        let errkind = err.kind();
                        if errkind == ErrorKind::WouldBlock {
                            // println!("WouldBlock {sock:?}");
                            return true;
                        } else if errkind == ErrorKind::ConnectionReset {
                            // closed
                            return false;
                        }
                        logmsg!(
                            "ERROR recv sock: {sock:?} error: {}, errkind: {errkind:?}",
                            err
                        );
                        return false;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    pub fn test_tcp_event_handler() {
        let addr = "127.0.0.1:12355";
        let mut mgr = SocketPoller::new();
        mgr.start_listen(
            addr,
            DefaultTcpListenerHandler::<sample::TcpEchoHandler>::new_boxed(),
        )
        .unwrap();
        mgr.start_connect(addr, sample::TcpEchoHandler::new_client(2, 1000))
            .unwrap();
        while mgr.count_streams() > 0 {
            mgr.process_events();
        }
        assert_eq!(mgr.len(), 1); // remaining listener
    }
}
