use crate::dbglog;
// import one macro per line. macros are exported at root of crate instead of mod level.
use crate::flat_storage::FlatStorage;
use crate::logmsg;
use crate::utils;
use polling::{Event, Events, PollMode, Poller};
use std::io::Write;
// TODO : investigate whether polling has implemented Poller with IOCP correctly ???
use std::io::{ErrorKind, Read};
use std::time::Duration;
use std::{marker::PhantomData, net::TcpStream};

//====================================================================================
//            Poller
//====================================================================================

pub trait TcpStreamHandler {
    fn on_connected(&mut self, sock: &mut std::net::TcpStream) -> bool;
    /// \return false to close socket.
    fn on_readable(&mut self, sock: &mut std::net::TcpStream) -> bool;
}

pub trait TcpListenerHandler {
    /// \return null to close new connection.
    fn on_new_connection(
        &mut self,
        sock: &mut std::net::TcpListener,
        new_sock: &mut std::net::TcpStream,
        addr: std::net::SocketAddr,
    ) -> Option<Box<dyn TcpStreamHandler>>;
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
    StreamType(std::net::TcpStream, Box<dyn TcpStreamHandler>, MsgSender),
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
        let key = self.socket_handlers.add(TcpSocketHandler::StreamType(
            sock,
            handler,
            MsgSender::new(),
        ));
        if let TcpSocketHandler::StreamType(ref sock, _, _) = self.socket_handlers.get(key).unwrap()
        {
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

    pub fn close_by_key(&mut self, key: SocketKey) {
        if let Some(sockhandler) = self.socket_handlers.get_mut(key.0) {
            match *sockhandler {
                TcpSocketHandler::StreamType(ref mut sock, _, ref sender) => {
                    logmsg!(
                        "removing key: {}, sock: {:?}, pending_send_bytes: {}",
                        key.0,
                        sock,
                        sender.buf.len()
                    );
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

            if let Some(sockhandler) = self.connections.socket_handlers.get_mut(ev.key) {
                match sockhandler {
                    TcpSocketHandler::ListenerType(ref mut sock, ref mut handler) => {
                        if ev.readable {
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
                        if ev.writable {
                            logmsg!("[ERROR] writable listener sock!");
                            removesock = true;
                        }
                    }
                    TcpSocketHandler::StreamType(
                        ref mut sock,
                        ref mut handler,
                        ref mut _sender,
                    ) => {
                        if ev.readable {
                            removesock = !handler.on_readable(sock);
                        }
                        if ev.writable {
                            // TODO, send remaining data in send_buf.
                            dbglog!("[WARN] NOT implemented writable sock {:?}!", sock);
                        }
                    }
                }
            } else {
                panic!("no event key!");
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
                self.connections.close_by_key(SocketKey(ev.key));
            }
        }

        return !self.events.is_empty();
    }
}

//====================================================================================
//            MsgSender
//====================================================================================

/// MsgSender is a per-socket object try sending msg on a non-blocking socket. if it fails due to WOULDBLOCK,
/// the unsent bytes are saved and register a Write insterest in poller, so that
/// the remaining data will be scheduled to send on next Writeable event.
struct MsgSender {
    pub buf: Vec<u8>,
    pub pending: FlatStorage<PendingSend>,
    first_pending_id: usize, // the id in flat_storage, usize::MAX is invalid
    last_pending_id: usize,  // the id in flat_storage, usize::MAX is invalid
    pub bytes_sent: usize,   // total bytes having been sent. buf[0] is bytes_sent+1 byte to send.
}
struct PendingSend {
    next_id: usize,  // the id in flat_storage
    startpos: usize, // the first byte of message to sent in buf,
    msgsize: usize,
    completion: Box<dyn Fn()>, // notify write completion.
}

enum SendOrQueResult {
    Complete,     // No message in queue
    Queue,        // message in queue
    CloseOrError, // close socket.
}
impl MsgSender {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            pending: FlatStorage::new(),
            first_pending_id: usize::MAX,
            last_pending_id: usize::MAX,
            bytes_sent: 0,
        }
    }
    pub fn send_or_que(
        &mut self,
        sock: &mut std::net::TcpStream,
        buf: &[u8],
        send_completion: impl Fn() + 'static,
    ) -> SendOrQueResult {
        if buf.is_empty() {
            send_completion();
            return SendOrQueResult::Complete;
        }
        if !self.buf.is_empty() {
            self.queue_msg(buf, send_completion);
            return SendOrQueResult::Queue;
        }
        // else try send. queue it if fails.
        let mut sentbytes = 0;
        match sock.write(buf) {
            std::io::Result::Ok(bytes) => {
                if bytes == 0 {
                    logmsg!("[ERROR] sock 0 bytes {sock:?}. close socket");
                    return SendOrQueResult::CloseOrError;
                } else if bytes < buf.len() {
                    sentbytes = bytes;
                    // return SendOrQueResult::Queue; // queued
                } else {
                    send_completion();
                    return SendOrQueResult::Complete; // sent
                }
            }
            std::io::Result::Err(err) => {
                let errkind = err.kind();
                if errkind == ErrorKind::WouldBlock {
                    // return SendOrQueResult::Queue; // queued
                } else if errkind == ErrorKind::ConnectionReset {
                    logmsg!("sock reset : {sock:?}. close socket");
                    return SendOrQueResult::CloseOrError; // socket closed
                } else if errkind == ErrorKind::Interrupted {
                    logmsg!("[WARN] sock Interrupted : {sock:?}. retry");
                    // TODO: queue
                    // return SendOrQueResult::Queue; // Interrupted is not an error. queue
                } else {
                    logmsg!("[ERROR]: read on sock {sock:?}, error: {err:?}");
                    return SendOrQueResult::CloseOrError;
                }
            }
        }
        //---- queue the remaining bytes
        self.queue_msg(&buf[sentbytes..], send_completion);
        return SendOrQueResult::Queue;
    }

    pub fn queue_msg(&mut self, buf: &[u8], send_completion: impl Fn() + 'static) {
        let prev_id = self.last_pending_id;
        self.last_pending_id = self.pending.add(PendingSend {
            next_id: usize::MAX,
            startpos: self.bytes_sent + self.buf.len(),
            msgsize: buf.len(),
            completion: Box::new(send_completion),
        });
        if let Some(prev) = self.pending.get_mut(prev_id) {
            prev.next_id = self.last_pending_id;
        }
        self.buf.extend_from_slice(buf);
    }

    #[allow(unused_assignments)]
    pub fn send_queued(&mut self, sock: &mut std::net::TcpStream) -> SendOrQueResult {
        if self.buf.is_empty() {
            return SendOrQueResult::Complete;
        }
        let mut sentbytes = 0;
        match sock.write(&self.buf[..]) {
            std::io::Result::Ok(bytes) => {
                sentbytes = bytes;
                if bytes == 0 {
                    logmsg!("[ERROR] sock 0 bytes {sock:?}. close socket");
                    return SendOrQueResult::CloseOrError;
                }
            }
            std::io::Result::Err(err) => {
                let errkind = err.kind();
                if errkind == ErrorKind::WouldBlock {
                    return SendOrQueResult::Queue; // queued
                } else if errkind == ErrorKind::ConnectionReset {
                    logmsg!("sock reset : {sock:?}. close socket");
                    return SendOrQueResult::CloseOrError; // socket closed
                } else if errkind == ErrorKind::Interrupted {
                    logmsg!("[WARN] sock Interrupted : {sock:?}. retry");
                    return SendOrQueResult::Queue; // Interrupted is not an error. queue
                } else {
                    logmsg!("[ERROR]: read on sock {sock:?}, error: {err:?}");
                    return SendOrQueResult::CloseOrError;
                }
            }
        }
        //-- now sent some bytes. notify
        let id = self.first_pending_id;
        while id != usize::MAX {
            let (mut sent, mut next_id) = (false, 0);
            if let Some(pending) = self.pending.get_mut(id) {
                if pending.startpos + pending.msgsize <= self.bytes_sent {
                    // fulled sent
                    sent = true;
                    next_id = pending.next_id;
                } else {
                    // the first msg not being fully sent.
                    pending.msgsize -= self.bytes_sent - pending.startpos;
                    pending.startpos = self.bytes_sent;
                    break;
                }
            } else {
                panic!("invalid id");
            }
            if sent {
                self.first_pending_id = next_id;
                if let Some(pending) = self.pending.remove(id) {
                    (pending.completion)();
                }
            }
        }
        //- move front buf
        let len = self.buf.len();
        self.buf.copy_within(sentbytes..len, 0);
        self.buf.resize(self.buf.len() - sentbytes, 0);
        if self.buf.is_empty() {
            // TODO: update buf setting
            return SendOrQueResult::Complete;
        } else {
            return SendOrQueResult::Queue;
        }
    }
}

//====================================================================================
//            MsgReader
//====================================================================================

pub trait MsgDispatcher {
    /// dispatch msg.
    /// @param msg_size  the decoded message size which is return value of previous call of dispatch. 0 means message having not been decoded.
    /// @return ExpectMsgSize(msgsize) to indicate more read until full msgsize is read then call next dispatch. msgsize==0 means msg size is unknown.
    ///         DropMsgSize(msgsize) to indiciate message is processed already. framework can drop the message after call. then msgsize will be 0 again.
    fn dispatch(
        &mut self,
        buf: &mut [u8],
        msg_size: usize,
        sock: &mut std::net::TcpStream,
    ) -> DispatchResult;
}
pub enum DispatchResult {
    ExpectMsgSize(usize), // expecting more read, indicating expected size, dispatcher will not be called until specified msg bytes are read. size==0 means unknown size.
    DropMsgSize(usize),   // msg has been dispatched, indicating bytes to drop.
}

/// Framework should repeatedly call MsgReader::try_read() to read messages from a sock into a recv_buffer and calls dispatcher to dispatch message.
///
pub struct MsgReader {
    recv_buffer: Vec<u8>,
    min_reserve: usize,
    startpos: usize,        // msg start position or first effective byte.
    bufsize: usize,         // count from buffer[0] to last read byte.
    decoded_msgsize: usize, // decoded msg size from DispatchResult::ExpectMsgSize,
}

impl MsgReader {
    pub fn new(min_recv_buf_reserve: usize) -> Self {
        Self {
            // dispatcher : dispatch,
            recv_buffer: Vec::new(),
            min_reserve: min_recv_buf_reserve,
            startpos: 0,
            bufsize: 0,
            decoded_msgsize: 0,
        }
    }

    /// On each new read, call callback dispatcher: (buffer, msg_size, sock) -> DispatchResult
    /// @param msg_size  the decoded message size which is return value of previous call of dispatch. 0 means message having not been decoded.
    /// @return ExpectMsgSize(msgsize) to indicate more read until full msgsize is read then call next dispatch. msgsize==0 means msg size is unknown.
    ///         DropMsgSize(msgsize) to indiciate message is processed already. framework can drop the message after call. then msgsize will be 0 again.
    ///
    fn try_read(
        &mut self,
        sock: &mut std::net::TcpStream,
        dispatcher: &mut impl MsgDispatcher,
    ) -> bool {
        loop {
            debug_assert!(
                self.decoded_msgsize == 0 || self.decoded_msgsize > self.bufsize - self.startpos
            ); // msg size is not decoded, or have not received expected msg size.

            if self.bufsize + self.min_reserve > self.recv_buffer.len() {
                self.recv_buffer.resize(self.bufsize + self.min_reserve, 0);
            }
            match sock.read(&mut self.recv_buffer[self.bufsize..]) {
                std::io::Result::Ok(nbytes) => {
                    if nbytes == 0 {
                        logmsg!("peer closed sock: {:?}", sock);
                        return false;
                    }
                    debug_assert!(self.bufsize + nbytes <= self.recv_buffer.len());

                    self.bufsize += nbytes;
                    let should_return = self.bufsize < self.recv_buffer.len(); // not full, no need to retry this time.
                    while self.startpos < self.bufsize
                        && (self.decoded_msgsize == 0
                            || self.startpos + self.decoded_msgsize <= self.bufsize)
                    {
                        match dispatcher.dispatch(
                            &mut self.recv_buffer[self.startpos..self.bufsize],
                            self.decoded_msgsize,
                            sock,
                        ) {
                            DispatchResult::ExpectMsgSize(msgsize) => {
                                assert!(
                                    msgsize == 0 || msgsize > self.bufsize - self.startpos,
                                    "{msgsize:?} startpos:{} bufsize: {}",
                                    self.startpos,
                                    self.bufsize
                                ); // either unknown msg size, or partial msg.
                                self.decoded_msgsize = msgsize; // could be 0 if msg size is unknown.
                            }
                            DispatchResult::DropMsgSize(msgsize) => {
                                assert!(msgsize > 0 && msgsize <= self.bufsize - self.startpos); // drop size should not exceed buffer size.
                                self.startpos += msgsize;
                                self.decoded_msgsize = 0;
                            }
                        }
                    }
                    if self.startpos != 0 {
                        // move front
                        self.recv_buffer.copy_within(self.startpos..self.bufsize, 0); // don't resize.
                        self.bufsize -= self.startpos;
                        self.startpos = 0;
                    }
                    if should_return {
                        // not full.
                        return true; // wait for next readable.
                    } else {
                        continue; // try next read.
                    }
                }
                std::io::Result::Err(err) => {
                    let errkind = err.kind();
                    if errkind == ErrorKind::WouldBlock {
                        return true; // wait for next readable.
                    } else if errkind == ErrorKind::ConnectionReset {
                        logmsg!("sock reset : {sock:?}. close socket");
                        return false; // socket closed
                    } else if errkind == ErrorKind::Interrupted {
                        logmsg!("[WARN] sock Interrupted : {sock:?}. retry");
                        return true; // Interrupted is not an error.
                    }
                    logmsg!("[ERROR]: read on sock {sock:?}, error: {err:?}");
                    return false;
                }
            }
        }
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

//====================================================================================
//            TcpEchoHandler
//====================================================================================

pub mod sample {
    use std::io::Write;

    use super::*;

    #[derive(Copy, Clone)]
    pub struct MsgHeader {
        body_len: u16,
        send_time: i64, // sending timestamp nanos since epoch
    }
    const MSG_HEADER_SIZE: usize = 10;
    const MIN_RECV_BUFFER_SIZE: usize = 1024;
    const LATENCY_BATCH_SIZE: i32 = 10000;

    pub struct TcpEchoHandler {
        reader: MsgReader,
        dispatcher: EchoAndLatency, // reader & dispacther must be decoupled, in order to avoid double mutable reference of self.
    }

    /// EchoAndLatency reports latency and echo back.
    /// It's owned by TcpEchoHandler/TcpStreamHandler, which also owns a MsgReader that calls EchoAndLatency::dispatch.
    /// MsgReader & MsgDispatcher must be decoupled.
    /// See issue: https://stackoverflow.com/questions/79015535/could-anybody-optimize-class-design-and-fix-issue-mutable-more-than-once-at-a-t
    struct EchoAndLatency {
        pub is_client: bool, // default false
        pub max_echo: i32,
        pub count_echo: i32,

        pub latency_batch: i32, // number of messages to report latencies.
        pub last_sent_time: i64,
        pub single_trip_durations: Vec<i64>,
        pub round_trip_durations: Vec<i64>,
    }
    impl MsgDispatcher for EchoAndLatency {
        fn dispatch(
            &mut self,
            buf: &mut [u8],
            msg_size: usize,
            sock: &mut std::net::TcpStream,
        ) -> DispatchResult {
            let mut msg_size = msg_size;
            if msg_size == 0 {
                // decode header
                if buf.len() < MSG_HEADER_SIZE {
                    return DispatchResult::ExpectMsgSize(0); // partial header
                }
                let header: &MsgHeader = utils::bytes_to_ref(&buf[0..MSG_HEADER_SIZE]);
                msg_size = header.body_len as usize + MSG_HEADER_SIZE;

                if msg_size > buf.len() {
                    return DispatchResult::ExpectMsgSize(msg_size); // decoded msg_size but still partial msg. need reading more.
                } // else having read full msg. should NOT return. continue processing.
            }
            debug_assert!(buf.len() >= msg_size); // full msg.
                                                  //---- process full message.
            let recvtime = utils::now_nanos();
            {
                let header: &MsgHeader = utils::bytes_to_ref(&buf[0..MSG_HEADER_SIZE]);
                debug_assert_eq!(header.body_len as usize + MSG_HEADER_SIZE, buf.len());

                if self.last_sent_time > 0 {
                    self.round_trip_durations
                        .push(recvtime - self.last_sent_time);
                    self.single_trip_durations.push(recvtime - header.send_time);
                    dbglog!(
                        "[{}, {}, {}] content: {} <{}>",
                        self.last_sent_time,
                        header.send_time,
                        recvtime,
                        buf.len(),
                        std::str::from_utf8(&buf[MSG_HEADER_SIZE..]).unwrap()
                    );
                }
            }
            // here drop header because report_latencies is one more but borrow.
            if self.round_trip_durations.len() as i32 == self.latency_batch {
                self.report_latencies();
            }
            let header: &mut MsgHeader = utils::bytes_to_ref_mut(&mut buf[0..MSG_HEADER_SIZE]);
            header.send_time = utils::now_nanos(); // update send_time only

            if self.count_echo < self.max_echo {
                let nsent = sock.write(&buf[..msg_size]).unwrap();
                debug_assert_eq!(nsent, msg_size);
                self.last_sent_time = utils::now_nanos();

                self.count_echo += 1;
            }
            return DispatchResult::DropMsgSize(msg_size);
        }
    }
    impl EchoAndLatency {
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
            let fact = 1;
            println!(
                "SingleTrip        {}       {}      {}      {}",
                d[0] / fact,
                d[n / 2] / fact,
                d[(n as f32 * 0.99) as usize] / fact,
                d[n - 2] / fact
            );

            let (d, n) = (
                &self.round_trip_durations[..],
                self.round_trip_durations.len(),
            );
            println!(
                "RoundTrip         {}       {}      {}      {}",
                d[0] / fact,
                d[n / 2] / fact,
                d[(n as f32 * 0.99) as usize] / fact,
                d[n - 2] / fact
            );
            self.single_trip_durations.clear();
            self.round_trip_durations.clear();
        }
    }

    impl Default for TcpEchoHandler {
        fn default() -> Self {
            TcpEchoHandler::new(false, i32::MAX, LATENCY_BATCH_SIZE)
        }
    }
    impl TcpEchoHandler {
        pub fn new(isclient: bool, a_max_echo: i32, a_latency_batch: i32) -> Self {
            Self {
                reader: MsgReader::new(MIN_RECV_BUFFER_SIZE),
                dispatcher: EchoAndLatency {
                    is_client: isclient,
                    max_echo: a_max_echo,
                    count_echo: 0,
                    latency_batch: a_latency_batch,
                    //
                    last_sent_time: 0,
                    single_trip_durations: Vec::new(),
                    round_trip_durations: Vec::new(),
                },
            }
        }

        pub fn new_client(max_echo: i32, latency_batch: i32) -> Box<Self> {
            Box::new(Self::new(true, max_echo, latency_batch))
        }
    }

    impl TcpStreamHandler for TcpEchoHandler {
        fn on_connected(&mut self, sock: &mut std::net::TcpStream) -> bool {
            if self.dispatcher.is_client {
                logmsg!("client sock: {:?} connected.", sock);
                let msg_content = b"test msg00";
                let mut buf = vec![0u8; msg_content.len() + MSG_HEADER_SIZE];

                buf[MSG_HEADER_SIZE..(MSG_HEADER_SIZE + msg_content.len())]
                    .copy_from_slice(&msg_content[..]);
                let header: &mut MsgHeader = utils::bytes_to_ref_mut(&mut buf[0..10]);
                header.body_len = msg_content.len() as u16;
                header.send_time = utils::now_nanos();

                //
                let nsent = sock
                    .write(&buf[..(msg_content.len() + MSG_HEADER_SIZE)])
                    .unwrap();
                debug_assert_eq!(nsent, msg_content.len() + MSG_HEADER_SIZE);
                dbglog!("client sent initial msg");
            } else {
                logmsg!("server sock: {sock:?} connected.");
            }
            return true;
        }

        fn on_readable(&mut self, sock: &mut std::net::TcpStream) -> bool {
            if !self.reader.try_read(sock, &mut self.dispatcher) {
                return false; // error: close
            }
            if self.dispatcher.count_echo >= self.dispatcher.max_echo {
                logmsg!(
                    "sock:{:?} reached max echo: {}. closing.",
                    sock,
                    self.dispatcher.max_echo
                );
                return false;
            }
            return true;
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
    #[test]
    pub fn test_send_or_que() {
        let _sender = MsgSender::new();
    }
}
