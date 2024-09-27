use crate::dbglog;
// import one macro per line. macros are exported at root of crate instead of mod level.
use crate::flat_storage::FlatStorage;
use crate::logmsg;
use crate::utils;
use core::panic;
use polling::{Event, Events, PollMode, Poller};
// TODO : investigate whether polling has implemented Poller with IOCP correctly ???
use std::io::{ErrorKind, Read, Write};
use std::time::Duration;
use std::{marker::PhantomData, net::TcpStream};

//====================================================================================
//            Poller
//====================================================================================

pub trait Reactor {
    fn on_connected(&mut self, ctx: &mut Context) -> bool;
    /// \return false to close socket.
    fn on_readable(&mut self, ctx: &mut Context) -> bool;
}

pub trait TcpListenerHandler {
    /// \return null to close new connection.
    fn on_new_connection(
        &mut self,
        sock: &mut std::net::TcpListener,
        new_sock: &mut std::net::TcpStream,
        addr: std::net::SocketAddr,
    ) -> Option<Box<dyn Reactor>>;
}

/// ReactRuntime manages TCP connections & polling.
pub struct ReactRuntime {
    mgr: ReactorMgr,
    events: Events, // decoupled events and connections to avoid double mutable refererence.
}

// Make a seperate struct ReactorMgr because when interating TcpConnectionMgr::events, sessions must be mutable in process_events.
struct ReactorMgr {
    socket_handlers: FlatStorage<TcpSocketHandler>,
    poller: Poller,
    count_streams: usize,            // TcpStreams only (excluding TcpListener)
    current_polling_sock: SocketKey, // the socket that is being processed in process_event function.
}

enum TcpSocketHandler {
    ListenerType(std::net::TcpListener, Box<dyn TcpListenerHandler>), // <sock, handler, key_in_flat_storage>
    StreamType(Context, Box<dyn Reactor>),
}
pub struct Context {
    pub sockkey: SocketKey,
    pub sock: std::net::TcpStream,
    pub sender: MsgSender,
    pub reader: MsgReader,
    interested_writable: bool,
}
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct SocketKey(usize); // identify a socket in ReactorMgr.
pub const INVALID_SOCKET_KEY: SocketKey = SocketKey(usize::MAX);

impl ReactorMgr {
    pub fn new() -> Self {
        Self {
            socket_handlers: FlatStorage::new(),
            poller: Poller::new().unwrap(),
            count_streams: 0,
            current_polling_sock: INVALID_SOCKET_KEY,
        }
    }
    /// \return number of all managed socks.
    pub fn len(&self) -> usize {
        self.socket_handlers.len()
    }
    pub fn add_stream<'a>(
        &'a mut self,
        sock: std::net::TcpStream,
        handler: Box<dyn Reactor>,
    ) -> (&'a mut Context, &'a mut Box<dyn Reactor>) {
        let key = self.socket_handlers.add(TcpSocketHandler::StreamType(
            Context {
                sockkey: INVALID_SOCKET_KEY,
                sock: sock,
                sender: MsgSender::new(),
                reader: MsgReader::default(),
                interested_writable: false,
            },
            handler,
        ));
        self.count_streams += 1;
        if let TcpSocketHandler::StreamType(ref mut sockdata, ref mut handler) =
            self.socket_handlers.get_mut(key).unwrap()
        {
            sockdata.sockkey = SocketKey(key);
            unsafe {
                self.poller
                    .add_with_mode(&sockdata.sock, Event::readable(key), PollMode::Level)
                    .unwrap();
            }
            logmsg!("Added TcpStream socketKey: {key:?}, sock: {:?}", sockdata.sock);
            return (sockdata, handler);
        }
        panic!("ERROR! Failed to get new added sockdata!");
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
            logmsg!("Added TcpListener socketKey: {key:?}, sock: {:?}", sock);
        }
        SocketKey(key)
    }

    pub fn close_by_key(&mut self, key: SocketKey) {
        if self.current_polling_sock == key {
            return; // it's being processed in process_event.
        }
        if let Some(sockhandler) = self.socket_handlers.remove(key.0) {
            match sockhandler {
                TcpSocketHandler::StreamType(sockdata, _) => {
                    logmsg!(
                        "removing key: {}, sock: {:?}, pending_send_bytes: {}",
                        key.0,
                        sockdata.sock,
                        sockdata.sender.buf.len()
                    );
                    self.count_streams -= 1;
                    self.poller.delete(sockdata.sock).unwrap();
                }
                TcpSocketHandler::ListenerType(sock, _) => {
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
        handler: Box<dyn Reactor>,
    ) -> std::io::Result<SocketKey> {
        let sockkey = {
            let socket = TcpStream::connect(remote_addr)?;
            socket.set_nonblocking(true)?; // FIXME: use blocking socket and each nonblocking read and blocking write.
            let (sockdata, handler) = self.add_stream(socket, handler);
            if handler.on_connected(sockdata) {
                return Result::Ok(sockdata.sockkey);
            }
            sockdata.sockkey
        };
        self.close_by_key(sockkey);
        Result::Ok(INVALID_SOCKET_KEY)
    }
}

impl Default for ReactRuntime {
    fn default() -> Self {
        Self::new()
    }
}
impl ReactRuntime {
    pub fn new() -> Self {
        Self {
            mgr: ReactorMgr::new(),
            events: Events::new(),
        }
    }
    /// \return all socks (listener & stream)
    pub fn len(&self) -> usize {
        self.mgr.len()
    }
    pub fn count_streams(&self) -> usize {
        self.mgr.count_streams
    }

    /// Start to listen on port. When call handler on new connection;
    /// The handler will create new stream socket handler for TCP stream.
    /// The new socket is set to non-blockingg and added to poller for waiting for ReadReady event.
    pub fn start_listen(
        &mut self,
        local_addr: &str,
        handler: Box<dyn TcpListenerHandler>,
    ) -> std::io::Result<SocketKey> {
        self.mgr.start_listen(local_addr, handler)
    }

    /// Try connect to remote. If succeeds, add the non-blockingg socket to poller and waiting for ReadReady event.
    pub fn start_connect(
        &mut self,
        remote_addr: &str,
        handler: Box<dyn Reactor>,
    ) -> std::io::Result<SocketKey> {
        self.mgr.start_connect(remote_addr, handler)
    }

    /// return true to indicate some events has been processed.
    pub fn process_events(&mut self) -> bool {
        self.events.clear();
        self.mgr
            .poller
            .wait(&mut self.events, Some(Duration::from_secs(0)))
            .unwrap(); // None duration means forever

        for ev in self.events.iter() {
            let mut removesock = false;
            self.mgr.current_polling_sock = SocketKey(ev.key);
            if let Some(sockhandler) = self.mgr.socket_handlers.get_mut(ev.key) {
                match sockhandler {
                    TcpSocketHandler::ListenerType(ref mut sock, ref mut handler) => {
                        if ev.readable {
                            let (mut newsock, addr) = sock.accept().unwrap();
                            if let Some(newhandler) =
                                handler.on_new_connection(sock, &mut newsock, addr)
                            {
                                newsock.set_nonblocking(true).unwrap();
                                let newsockkey_to_close = {
                                    let (newsockdata, newhandler) =
                                        self.mgr.add_stream(newsock, newhandler);
                                    if !newhandler.on_connected(newsockdata) {
                                        newsockdata.sockkey
                                    } else {
                                        INVALID_SOCKET_KEY
                                    }
                                };
                                if newsockkey_to_close != INVALID_SOCKET_KEY {
                                    self.mgr.close_by_key(newsockkey_to_close);
                                }
                            }
                            // else newsock will auto destroy
                        }
                        if ev.writable {
                            logmsg!("[ERROR] writable listener sock!");
                            removesock = true;
                        }
                    }
                    TcpSocketHandler::StreamType(ref mut ctx, ref mut handler) => {
                        if ev.writable {
                            if !ctx.interested_writable {
                                dbglog!("WARN: unsolicited writable sock: {:?}", ctx.sock);
                            }
                            ctx.interested_writable = true; // in case unsolicited event.
                            if ctx.sender.pending.len() > 0 {
                                if SendOrQueResult::CloseOrError == ctx.sender.send_queued(&mut ctx.sock) {
                                    removesock = true;
                                }
                            }
                        }
                        if ev.readable {
                            removesock = !handler.on_readable(ctx);
                        }
                        if ctx.sender.close_or_error {
                            removesock = true;
                        }
                        // add or remove write interest
                        if !removesock {
                            if !ctx.interested_writable && ctx.sender.pending.len() > 0 {
                                self.mgr
                                    .poller
                                    .modify_with_mode(
                                        &ctx.sock,
                                        Event::all(ctx.sockkey.0),
                                        PollMode::Level,
                                    )
                                    .unwrap();
                                ctx.interested_writable = true;
                            } else if ctx.interested_writable
                                && ctx.sender.pending.len() == 0
                            {
                                self.mgr
                                    .poller
                                    .modify_with_mode(
                                        &ctx.sock,
                                        Event::readable(ctx.sockkey.0),
                                        PollMode::Level,
                                    )
                                    .unwrap();
                                ctx.interested_writable = false;
                            }
                        }
                    }
                }
            } else {
                dbglog!("socket key has been removed {}!", ev.key);
            }
            self.mgr.current_polling_sock = INVALID_SOCKET_KEY; // reset
            
            if removesock {
                self.mgr.close_by_key(SocketKey(ev.key));
                continue; // ignore error events.
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
                self.mgr.close_by_key(SocketKey(ev.key));
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
pub struct MsgSender {
    pub buf: Vec<u8>,
    pub pending: FlatStorage<PendingSend>,
    first_pending_id: usize, // the id in flat_storage, usize::MAX is invalid
    last_pending_id: usize,  // the id in flat_storage, usize::MAX is invalid
    pub bytes_sent: usize,   // total bytes having been sent. buf[0] is bytes_sent+1 byte to send.
    close_or_error: bool,
}


pub struct PendingSend {
    next_id: usize,  // the id in flat_storage
    startpos: usize, // the first byte of message to sent in buf,
    msgsize: usize,
    completion: Box<dyn Fn()>, // notify write completion.
}

#[derive(PartialEq, Eq)]
pub enum SendOrQueResult {
    Complete,             // No message in queue
    InQueue,              // message in queue
    CloseOrError, // close socket.
}
impl MsgSender {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            pending: FlatStorage::new(),
            first_pending_id: usize::MAX, // FIFO queue, append to last. pop from first.
            last_pending_id: usize::MAX,
            bytes_sent: 0,
            close_or_error: false,
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
            return SendOrQueResult::InQueue;
        }
        // else try send. queue it if fails.
        let mut sentbytes = 0;
        match sock.write(buf) {
            std::io::Result::Ok(bytes) => {
                if bytes == 0 {
                    logmsg!("[ERROR] sock 0 bytes {sock:?}. close socket");
                    self.close_or_error = true;
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
                    self.close_or_error = true;
                    return SendOrQueResult::CloseOrError;
                // socket closed
                } else if errkind == ErrorKind::Interrupted {
                    logmsg!("[WARN] sock Interrupted : {sock:?}. retry");
                    // return SendOrQueResult::Queue; // Interrupted is not an error. queue
                } else {
                    logmsg!("[ERROR]: write on sock {sock:?}, error: {err:?}");
                    self.close_or_error = true;
                    return SendOrQueResult::CloseOrError;
                }
            }
        }
        //---- queue the remaining bytes
        self.queue_msg(&buf[sentbytes..], send_completion);
        return SendOrQueResult::InQueue;
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
        if self.first_pending_id == usize::MAX {
            // add the first one
            self.first_pending_id = self.last_pending_id;
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
                    self.close_or_error = true;
                    return SendOrQueResult::CloseOrError;
                }
            }
            std::io::Result::Err(err) => {
                let errkind = err.kind();
                if errkind == ErrorKind::WouldBlock {
                    return SendOrQueResult::InQueue; // queued
                } else if errkind == ErrorKind::ConnectionReset {
                    logmsg!("sock reset : {sock:?}. close socket");
                    self.close_or_error = true;
                    return SendOrQueResult::CloseOrError;
                // socket closed
                } else if errkind == ErrorKind::Interrupted {
                    logmsg!("[WARN] sock Interrupted : {sock:?}. retry");
                    return SendOrQueResult::InQueue; // Interrupted is not an error. queue
                } else {
                    logmsg!("[ERROR]: write on sock {sock:?}, error: {err:?}");
                    self.close_or_error = true;
                    return SendOrQueResult::CloseOrError;
                }
            }
        }
        //-- now sent some bytes. notify
        while self.first_pending_id != usize::MAX {
            let id = self.first_pending_id;
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
        if self.first_pending_id == usize::MAX {
            // removed the last
            self.last_pending_id = usize::MAX;
        }
        //- move front buf
        let len = self.buf.len();
        self.buf.copy_within(sentbytes..len, 0);
        self.buf.resize(self.buf.len() - sentbytes, 0);
        if self.buf.is_empty() {
            // reset members
            debug_assert_eq!(self.first_pending_id, usize::MAX);
            debug_assert_eq!(self.last_pending_id, usize::MAX);
            debug_assert_eq!(self.pending.len(), 0);
            self.bytes_sent = 0;
            return SendOrQueResult::Complete;
        } else {
            self.bytes_sent += sentbytes;
            return SendOrQueResult::InQueue;
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
        ctx: &mut DispatchContext,
    ) -> DispatchResult;
}

pub struct DispatchContext<'a> {
    pub sock: &'a mut std::net::TcpStream,
    pub sender: &'a mut MsgSender,
}

pub enum DispatchResult {
    Error,
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

impl Default for MsgReader {
    fn default() -> Self {
        Self::new(1024)
    }
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
    fn try_read(&mut self, ctx: &mut DispatchContext, dispatcher: &mut impl MsgDispatcher) -> bool {
        loop {
            debug_assert!(
                self.decoded_msgsize == 0 || self.decoded_msgsize > self.bufsize - self.startpos
            ); // msg size is not decoded, or have not received expected msg size.

            if self.bufsize + self.min_reserve > self.recv_buffer.len() {
                self.recv_buffer.resize(self.bufsize + self.min_reserve, 0);
            }
            match ctx.sock.read(&mut self.recv_buffer[self.bufsize..]) {
                std::io::Result::Ok(nbytes) => {
                    if nbytes == 0 {
                        logmsg!("peer closed sock: {:?}", ctx.sock);
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
                            ctx,
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
                            DispatchResult::Error => {
                                logmsg!("[WARN] Dispatch error closing sock: {:?}. ", ctx.sock);
                                return false;
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
                        logmsg!("sock reset : {:?}. close socket", ctx.sock);
                        return false; // socket closed
                    } else if errkind == ErrorKind::Interrupted {
                        logmsg!("[WARN] sock Interrupted : {:?}. retry", ctx.sock);
                        return true; // Interrupted is not an error.
                    }
                    logmsg!("[ERROR]: read on sock {:?}, error: {err:?}", ctx.sock);
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

impl<StreamHandler: Reactor + Default + 'static> DefaultTcpListenerHandler<StreamHandler> {
    pub fn new_boxed() -> Box<Self> {
        Box::new(Self::default())
    }
}
impl<StreamHandler: Reactor + Default + 'static> Default
    for DefaultTcpListenerHandler<StreamHandler>
{
    fn default() -> Self {
        Self {
            _phantom: PhantomData::default(),
        }
    }
}
impl<StreamHandler: Reactor + Default + 'static> TcpListenerHandler
    for DefaultTcpListenerHandler<StreamHandler>
{
    fn on_new_connection(
        &mut self,
        _conn: &mut std::net::TcpListener,
        _new_conn: &mut std::net::TcpStream,
        _addr: std::net::SocketAddr,
    ) -> Option<Box<dyn Reactor>> {
        Some(Box::new(StreamHandler::default()))
    }
}

//====================================================================================
//            MyReactor
//====================================================================================

pub mod sample {
    use super::*;

    #[derive(Copy, Clone)]
    pub struct MsgHeader {
        body_len: u16,
        send_time: i64, // sending timestamp nanos since epoch
    }
    const MSG_HEADER_SIZE: usize = 10;
    const LATENCY_BATCH_SIZE: i32 = 10000;

    pub struct MyReactor {
        dispatcher: EchoAndLatency, // reader & dispacther must be decoupled, in order to avoid double mutable reference of self.
    }

    /// EchoAndLatency reports latency and echo back.
    /// It's owned by MyReactor, which also owns a MsgReader that calls EchoAndLatency::dispatch.
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
            ctx: &mut DispatchContext,
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
                        "Recv msg [{}, {}, {}] content: {} <{}>",
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
                if SendOrQueResult::CloseOrError == ctx.sender.send_or_que(ctx.sock, &buf[..msg_size], ||{}) {

                    return DispatchResult::Error;
                }
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
            let fact = 1000;
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

    impl Default for MyReactor {
        fn default() -> Self {
            MyReactor::new(false, i32::MAX, LATENCY_BATCH_SIZE)
        }
    }
    impl MyReactor {
        pub fn new(isclient: bool, a_max_echo: i32, a_latency_batch: i32) -> Self {
            Self {
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

    impl Reactor for MyReactor {
        fn on_connected(&mut self, ctx: &mut Context) -> bool {
            if self.dispatcher.is_client {
                logmsg!("client sock: {:?} connected.", ctx.sock);
                let msg_content = b"test msg00";
                let mut buf = vec![0u8; msg_content.len() + MSG_HEADER_SIZE];

                buf[MSG_HEADER_SIZE..(MSG_HEADER_SIZE + msg_content.len())]
                    .copy_from_slice(&msg_content[..]);
                let header: &mut MsgHeader = utils::bytes_to_ref_mut(&mut buf[0..10]);
                header.body_len = msg_content.len() as u16;
                header.send_time = utils::now_nanos();

                //
                let res = ctx.sender.send_or_que(&mut ctx.sock, &buf[..(msg_content.len() + MSG_HEADER_SIZE)], ||{});
                if res == SendOrQueResult::CloseOrError {
                    return false;
                }
                dbglog!("client sent initial msg");
            } else {
                logmsg!("server sock: {:?} connected.", ctx.sock);
            }
            return true;
        }

        fn on_readable(&mut self, ctx: &mut Context) -> bool {
            if !ctx.reader.try_read(
                &mut DispatchContext {
                    sock: &mut ctx.sock,
                    sender: &mut ctx.sender,
                },
                &mut self.dispatcher,
            ) {
                return false; // error: close
            }
            if self.dispatcher.count_echo >= self.dispatcher.max_echo {
                logmsg!(
                    "sock:{:?} reached max echo: {}. closing.",
                    ctx.sock,
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
        let mut runtime = ReactRuntime::new();
        runtime.start_listen(
            addr,
            DefaultTcpListenerHandler::<sample::MyReactor>::new_boxed(),
        )
        .unwrap();
    runtime.start_connect(addr, sample::MyReactor::new_client(2, 1000))
            .unwrap();
        while runtime.count_streams() > 0 {
            runtime.process_events();
        }
        assert_eq!(runtime.len(), 1); // remaining listener
    }
    #[test]
    pub fn test_send_or_que() {
        let _sender = MsgSender::new();
    }
}
