use crate::dbglog;
// import one macro per line. macros are exported at root of crate instead of mod level.
use crate::flat_storage::FlatStorage;
use crate::logmsg;
use crate::utils;
use core::panic;
use polling::{Event, Events, PollMode, Poller};
use std::io::{ErrorKind, Read, Write};
use std::time::Duration;
use std::{marker::PhantomData, net::TcpStream};

//====================================================================================
//            Reactor, Poller
//====================================================================================

/// A Reactor is assigned a unique ReactorID by ReactRuntime, and is able to receive socket messsages (via reader) and commands.
/// Besides socket communication, Sending command is the only thread-safe way to communicate with a Reactor.
/// A Reactor could send socket messages (via sender), and send commands (via cmd_sender) to another Reactor with specific ReactorID.
pub trait Reactor {
    type UserCommand;

    /// called when connection is established.
    /// @param listener the listener ID when the reactor is created by a listener socket; otherwise, it's INVALID_REACTOR_ID.
    fn on_connected(
        &mut self,
        ctx: &mut ReactorContext<Self::UserCommand>,
        listener: ReactorID,
    ) -> bool;

    /// \return false to close socket.
    fn on_readable(&mut self, ctx: &mut ReactorContext<Self::UserCommand>) -> bool;

    fn on_command(&mut self, cmd: Self::UserCommand, ctx: &mut ReactorContext<Self::UserCommand>);

    /// called when the reactor is removed from poller and before closing the socket.
    /// The Reactor will be destroyed after this call.
    fn on_close(&mut self, _cmd_sender: &CmdSender<Self::UserCommand>) {}
}

pub type CmdSender<UserCommand> = std::sync::mpsc::Sender<CmdData<UserCommand>>;
pub struct ReactorContext<'a, UserCommand> {
    pub sockkey: SocketKey,
    pub sock: &'a mut std::net::TcpStream,
    pub sender: &'a mut MsgSender, // sock_sender
    pub reader: &'a mut MsgReader, // sock_reader
    pub cmd_sender: &'a CmdSender<UserCommand>,
}
pub trait TcpListenerHandler {
    type UserCommand;
    /// \return null to close new connection.
    fn on_new_connection(
        &mut self,
        sock: &mut std::net::TcpListener,
        new_sock: &mut std::net::TcpStream,
        addr: std::net::SocketAddr,
    ) -> Option<Box<dyn Reactor<UserCommand = Self::UserCommand>>>;

    fn on_close(&mut self, _cmd_sender: &CmdSender<Self::UserCommand>) {}
}

/// ReactRuntime manages Reactors which receive socket data or command.
/// A ReactRuntime has a command queue, defered command queue and a collection of reactors.
/// Each reactor is assigned a ReactorID when adding to ReactRuntime. Users send command to a reactor
/// with a specific ReactorID that belongs to the ReactRuntime. If it's INVALID_REACTOR_ID,
/// The command will not be passed to a reactor. The command could be immediate or defered for a time.
/// A example is that, on close of a reactor, it could send a command to the ReactRuntime to reconnect.
///
/// Communication between ReactRuntimes are via sending command also, which is the only thread-safe way.
/// Note that a ReactRuntime can only exist in a thread. Accessing a ReactRuntime from multiple thread is not supported.
/// Also multiple ReactRuntime in a thread is feasible, but is not needed.
pub struct ReactRuntime<UserCommand> {
    mgr: ReactorMgr<UserCommand>,
    defered_data: FlatStorage<CmdData<UserCommand>>,
    defered_heap: Vec<DeferedKey>, // min heap of (scheduled_time_nanos, Cmd_index_in_defered_data)
    events: Events, // decoupled events and connections to avoid double mutable refererence.
}

#[derive(Copy, Clone)]
struct DeferedKey {
    millis: i64,
    data: usize,
}
impl DeferedKey {
    fn get_key(&self) -> i64 {
        return self.millis;
    }
}

// push the last element to a min heap. sift up
fn min_heap_push(v: &mut [DeferedKey]) {
    let mut k = v.len() - 1; // last element
    let mut parent = (k - 1) / 2;
    while k > 0 && v[k].get_key() < v[parent].get_key() {
        v.swap(k, parent);
        k = parent;
        parent = (k - 1) / 2;
    }
}
// pop the first element to end. sift down.
fn min_heap_pop(v: &mut [DeferedKey]) {
    let mut k = 0;
    let value = v[0];
    while k < v.len() - 1 {
        let (l, r) = ((k + 1) * 2 - 1, (k + 1) * 2);
        let min = if r < v.len() - 1 {
            if v[l].get_key() < v[r].get_key() {
                l
            } else {
                r
            }
        } else if l < v.len() - 1 {
            l
        } else {
            break;
        };
        v.swap(min, k);
        k = min;
    }
    v[v.len() - 1] = value;
}

// Make a seperate struct ReactorMgr because when interating TcpConnectionMgr::events, sessions must be mutable in process_events.
struct ReactorMgr<UserCommand> {
    socket_handlers: FlatStorage<TcpSocketHandler<UserCommand>>,
    poller: Poller,
    count_streams: usize,            // TcpStreams only (excluding TcpListener)
    current_polling_sock: SocketKey, // the socket that is being processed in process_event function.
    cmd_recv: std::sync::mpsc::Receiver<CmdData<UserCommand>>,
    cmd_sender: CmdSender<UserCommand>,
}

enum TcpSocketHandler<UserCommand> {
    ListenerType(
        std::net::TcpListener,
        Box<dyn TcpListenerHandler<UserCommand = UserCommand>>,
    ), // <sock, handler, key_in_flat_storage>
    StreamType(SockData, Box<dyn Reactor<UserCommand = UserCommand>>),
}
pub struct SockData {
    pub sockkey: SocketKey,
    pub sock: std::net::TcpStream,
    pub sender: MsgSender,
    pub reader: MsgReader,
    interested_writable: bool,
}
impl<'a, UserCommand> ReactorContext<'a, UserCommand> {
    fn from(data: &'a mut SockData, cmd_sender: &'a CmdSender<UserCommand>) -> Self {
        Self {
            sockkey: data.sockkey,
            sock: &mut data.sock,
            sender: &mut data.sender,
            reader: &mut data.reader,
            cmd_sender: cmd_sender,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub struct SocketKey(usize); // identify a socket in ReactorMgr.
pub const INVALID_SOCKET_KEY: SocketKey = SocketKey(usize::MAX);
pub type ReactorID = SocketKey;
pub const INVALID_REACTOR_ID: ReactorID = INVALID_SOCKET_KEY;

impl<UserCommand> ReactorMgr<UserCommand> {
    pub fn new() -> Self {
        let (cmd_sender, cmd_recv) = std::sync::mpsc::channel::<CmdData<UserCommand>>();
        Self {
            socket_handlers: FlatStorage::new(),
            poller: Poller::new().unwrap(),
            count_streams: 0,
            current_polling_sock: INVALID_SOCKET_KEY,
            cmd_sender: cmd_sender,
            cmd_recv: cmd_recv,
        }
    }
    /// \return number of all managed socks.
    pub fn len(&self) -> usize {
        self.socket_handlers.len()
    }
    pub fn add_stream<'a>(
        &'a mut self,
        sock: std::net::TcpStream,
        handler: Box<dyn Reactor<UserCommand = UserCommand>>,
    ) -> SocketKey {
        let key = self.socket_handlers.add(TcpSocketHandler::StreamType(
            SockData {
                sockkey: INVALID_SOCKET_KEY,
                sock: sock,
                sender: MsgSender::new(),
                reader: MsgReader::default(),
                interested_writable: false,
            },
            handler,
        ));
        self.count_streams += 1;
        if let TcpSocketHandler::StreamType(ref mut sockdata, ref mut _handler) =
            self.socket_handlers.get_mut(key).unwrap()
        {
            sockdata.sockkey = SocketKey(key);
            unsafe {
                self.poller
                    .add_with_mode(&sockdata.sock, Event::readable(key), PollMode::Level)
                    .unwrap();
            }
            logmsg!(
                "Added TcpStream socketKey: {key:?}, sock: {:?}",
                sockdata.sock
            );
            return SocketKey(key);
        }
        panic!("ERROR! Failed to get new added sockdata!");
    }

    pub fn add_listener(
        &mut self,
        sock: std::net::TcpListener,
        handler: Box<dyn TcpListenerHandler<UserCommand = UserCommand>>,
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

    /// Close & remove socket/reactor.
    /// @return true if key exists ; false when key doesn't exist or it's in process of polling.
    pub fn close_by_key(&mut self, key: SocketKey) -> bool {
        if self.current_polling_sock == key {
            return false; // it's being processed in process_event.
        }
        if let Some(sockhandler) = self.socket_handlers.remove(key.0) {
            match sockhandler {
                TcpSocketHandler::StreamType(sockdata, mut reactor) => {
                    logmsg!(
                        "removing key: {}, sock: {:?}, pending_send_bytes: {}",
                        key.0,
                        sockdata.sock,
                        sockdata.sender.buf.len()
                    );
                    self.count_streams -= 1;
                    self.poller.delete(&sockdata.sock).unwrap();
                    (reactor).on_close(&self.cmd_sender);
                }
                TcpSocketHandler::ListenerType(sock, mut reactor) => {
                    logmsg!("removing key: {}, sock: {:?}", key.0, sock);
                    self.poller.delete(&sock).unwrap();
                    (reactor).on_close(&self.cmd_sender);
                }
            }
            return true;
        }
        return false;
    }

    /// \local_addr  ip:port. e.g. "127.0.0.1:8000"
    pub fn start_listen(
        &mut self,
        local_addr: &str,
        handler: Box<dyn TcpListenerHandler<UserCommand = UserCommand>>,
    ) -> std::io::Result<SocketKey> {
        let socket = std::net::TcpListener::bind(local_addr)?;
        socket.set_nonblocking(true)?;
        Result::Ok(self.add_listener(socket, handler))
    }

    pub fn start_connect(
        &mut self,
        remote_addr: &str,
        handler: Box<dyn Reactor<UserCommand = UserCommand>>,
    ) -> std::io::Result<SocketKey> {
        let sockkey = {
            let socket = TcpStream::connect(remote_addr)?;
            socket.set_nonblocking(true)?; // FIXME: use blocking socket and each nonblocking read and blocking write.
            let sockkey = self.add_stream(socket, handler);
            if let TcpSocketHandler::StreamType(ref mut sockdata, ref mut handler) =
                self.socket_handlers.get_mut(sockkey.0).unwrap()
            {
                if handler.on_connected(
                    &mut ReactorContext::from(sockdata, &self.cmd_sender),
                    INVALID_REACTOR_ID,
                ) {
                    return Result::Ok(sockdata.sockkey);
                }
            }
            sockkey
        };
        self.close_by_key(sockkey);
        Result::Ok(INVALID_SOCKET_KEY)
    }
}

impl<UserCommand> Default for ReactRuntime<UserCommand> {
    fn default() -> Self {
        Self::new()
    }
}
impl<UserCommand> ReactRuntime<UserCommand> {
    pub fn new() -> Self {
        Self {
            mgr: ReactorMgr::new(),
            defered_data: FlatStorage::new(),
            defered_heap: Vec::new(),
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
    pub fn get_cmd_sender(&self) -> &CmdSender<UserCommand> {
        &self.mgr.cmd_sender
    }
    pub fn send_cmd(
        &self,
        reactorid: ReactorID,
        cmd: SysCommand<UserCommand>,
        deferred: Deferred,
        completion: impl FnOnce(CommandCompletion) + 'static,
    ) -> bool {
        return self
            .mgr
            .cmd_sender
            .send_cmd(reactorid, cmd, deferred, completion);
    }

    /// Start to listen on port. When call handler on new connection;
    /// The handler will create new stream socket handler for TCP stream.
    /// The new socket is set to non-blockingg and added to poller for waiting for ReadReady event.
    pub fn start_listen(
        &mut self,
        local_addr: &str,
        handler: Box<dyn TcpListenerHandler<UserCommand = UserCommand>>,
    ) -> std::io::Result<SocketKey> {
        self.mgr.start_listen(local_addr, handler)
    }

    /// Try connect to remote. If succeeds, add the non-blockingg socket to poller and waiting for ReadReady event.
    pub fn start_connect(
        &mut self,
        remote_addr: &str,
        handler: Box<dyn Reactor<UserCommand = UserCommand>>,
    ) -> std::io::Result<SocketKey> {
        self.mgr.start_connect(remote_addr, handler)
    }

    /// return true to indicate some events has been processed.
    fn process_sock_events(&mut self) -> bool {
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
                                    let newsockkey = self.mgr.add_stream(newsock, newhandler);
                                    if let TcpSocketHandler::StreamType(
                                        ref mut newsockdata,
                                        ref mut newhandler,
                                    ) = self.mgr.socket_handlers.get_mut(newsockkey.0).unwrap()
                                    {
                                        if !newhandler.on_connected(
                                            &mut ReactorContext::from(
                                                newsockdata,
                                                &self.mgr.cmd_sender,
                                            ),
                                            SocketKey(ev.key),
                                        ) {
                                            newsockdata.sockkey
                                        } else {
                                            INVALID_SOCKET_KEY
                                        }
                                    } else {
                                        panic!("Failed to find new added stream!");
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
                                if SendOrQueResult::CloseOrError
                                    == ctx.sender.send_queued(&mut ctx.sock)
                                {
                                    removesock = true;
                                }
                            }
                        }
                        if ev.readable {
                            removesock = !handler
                                .on_readable(&mut ReactorContext::from(ctx, &self.mgr.cmd_sender));
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
                            } else if ctx.interested_writable && ctx.sender.pending.len() == 0 {
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

    /// @return number of command procesed
    fn process_command_queue(&mut self) -> usize {
        let mut count_cmd = 0usize;
        loop {
            let cmddata: CmdData<UserCommand> = match self.mgr.cmd_recv.try_recv() {
                Err(err) => {
                    if err == std::sync::mpsc::TryRecvError::Empty {
                        return count_cmd;
                    } else {
                        panic!("std::sync::mpsc::TryRecvError::Disconnected is not possible. Because both cmd_sender & cmd_recv are saved.");
                    }
                }
                Ok(data) => data,
            };
            count_cmd += 1;

            match cmddata.deferred {
                Deferred::Immediate => {}
                Deferred::UtilTime(time) => {
                    let millis = time
                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as i64;
                    if !ReactRuntime::<UserCommand>::is_defered_current(millis) {
                        // if beyond half millis tolerance
                        let key = self.defered_data.add(cmddata);
                        self.defered_heap.push(DeferedKey {
                            millis: millis,
                            data: key,
                        });
                        min_heap_push(&mut self.defered_heap);
                        continue; // continue loop recv
                    }
                }
            }

            self.execute_immediate_cmd(cmddata);
        } // loop
    }

    fn is_defered_current(millis: i64) -> bool {
        let now_nanos = utils::now_nanos();
        return millis * 1000000 + 5 * 100000 <= now_nanos;
    }

    fn execute_immediate_cmd(&mut self, cmddata: CmdData<UserCommand>) {
        match cmddata.cmd {
            SysCommand::NewConnect(remote_addr, reactor) => {
                match self.mgr.start_connect(&remote_addr, reactor) {
                    Err(err) => {
                        let errmsg =
                            format!("Failed to connect to {}. Error: {}", remote_addr, err);
                        (cmddata.completion)(CommandCompletion::Error(errmsg));
                    }
                    Ok(key) => {
                        (cmddata.completion)(CommandCompletion::Completed(key));
                    }
                }
            }
            SysCommand::NewListen(local_addr, reactor) => {
                match self.mgr.start_listen(&local_addr, reactor) {
                    Err(err) => {
                        let errmsg = format!("Failed to listen on {}. Error: {}", local_addr, err);
                        (cmddata.completion)(CommandCompletion::Error(errmsg));
                    }
                    Ok(key) => {
                        (cmddata.completion)(CommandCompletion::Completed(key));
                    }
                }
            }
            SysCommand::CloseSocket => {
                if self.mgr.close_by_key(cmddata.reactorid) {
                    (cmddata.completion)(CommandCompletion::Completed(cmddata.reactorid));
                } else {
                    (cmddata.completion)(CommandCompletion::Error(format!(
                        "Failed to remove non existing socket with key: {}",
                        cmddata.reactorid.0
                    )));
                }
            }
            SysCommand::UserCmd(usercmd) => {
                if cmddata.reactorid == INVALID_REACTOR_ID {
                    panic!("UserCommand must be executed on a reactor!");
                } else {
                    if let Some(handler) = self.mgr.socket_handlers.get_mut(cmddata.reactorid.0) {
                        match handler {
                            TcpSocketHandler::ListenerType(_, _) => {
                                (cmddata.completion)(CommandCompletion::Error(format!(
                                    "Listener cannot receive user command. reactorid: {}",
                                    cmddata.reactorid.0
                                )));
                            }
                            TcpSocketHandler::StreamType(ref mut ctx, ref mut reactor) => {
                                (reactor).on_command(
                                    usercmd,
                                    &mut ReactorContext::from(ctx, &self.mgr.cmd_sender),
                                );
                                (cmddata.completion)(CommandCompletion::Completed(
                                    cmddata.reactorid,
                                ));
                            }
                        }
                    } else {
                        (cmddata.completion)(CommandCompletion::Error(format!(
                            "Failed to execute user command on non existing socket with key: {}",
                            cmddata.reactorid.0
                        )));
                    }
                }
            }
        } // match cmd
    }

    fn process_defered_queue(&mut self) -> usize {
        while self.defered_heap.len() > 0
            && ReactRuntime::<UserCommand>::is_defered_current(self.defered_heap[0].millis)
        {
            let key = self.defered_heap[0].data;
            min_heap_pop(&mut self.defered_heap);
            self.defered_heap.pop();
            if let Some(cmddata) = self.defered_data.remove(key) {
                self.execute_immediate_cmd(cmddata);
            } else {
                panic!("No defered CommandData with key: {}", key);
            }
        }
        return 0;
    }

    /// This function should be periodically called to process socket messages and commands.
    /// @return true if the runtime has reactor or command;
    ///     false when there's no reactor/socket or command, then this runtime could be destroyed.
    ///     But cloned senders may still send cmd before destruction. User must handle this race condition.
    pub fn process_events(&mut self) -> bool {
        let has_events = self.process_sock_events();
        let defereds = self.process_defered_queue();
        let cmds = self.process_command_queue();
        return has_events
            || defereds > 0
            || cmds > 0
            || self.defered_heap.len() > 0
            || self.len() > 0;
        // TODO or there are defered commands.
    }
}

//====================================================================================
//            SysCommand to Reactor
//====================================================================================

pub enum SysCommand<UserCommand> {
    //-- system commands are processed by Runtime, Reactor will not receive them.
    NewConnect(String, Box<dyn Reactor<UserCommand = UserCommand>>), // connect to remote IP:Port
    NewListen(
        String,
        Box<dyn TcpListenerHandler<UserCommand = UserCommand>>,
    ), // listen on IP:Port
    CloseSocket,
    UserCmd(UserCommand),
}

pub enum Deferred {
    Immediate,
    UtilTime(std::time::SystemTime),
}
pub enum CommandCompletion {
    Completed(SocketKey),
    Error(String),
}

pub struct CmdData<UserCommand> {
    reactorid: ReactorID,
    cmd: SysCommand<UserCommand>,
    deferred: Deferred,
    completion: Box<dyn FnOnce(CommandCompletion)>,
}
unsafe impl<UserCommand> Send for CmdData<UserCommand> {}

trait ReactorCmdSender<UserCommand> {
    fn send_cmd(
        &self,
        reactorid: ReactorID,
        cmd: SysCommand<UserCommand>,
        deferred: Deferred,
        completion: impl FnOnce(CommandCompletion) + 'static,
    ) -> bool;
}

impl<UserCommand> ReactorCmdSender<UserCommand> for CmdSender<UserCommand> {
    fn send_cmd(
        &self,
        reactorid: ReactorID,
        cmd: SysCommand<UserCommand>,
        deferred: Deferred,
        completion: impl FnOnce(CommandCompletion) + 'static,
    ) -> bool {
        // check NewConnect/NewListen when reactor == INVALID.
        match &cmd {
            SysCommand::NewListen(_, _) | SysCommand::NewConnect(_, _) => {
                if reactorid != INVALID_REACTOR_ID {
                    logmsg!("reactorid msut be INVALID_REACTOR_ID if NewConnect/NewListen");
                    return false;
                }
            }
            SysCommand::UserCmd(_) => {
                if reactorid == INVALID_REACTOR_ID {
                    logmsg!("UserCmd must has a valid reactorid.");
                    return false;
                }
            }
            _ => {}
        }
        self.send(CmdData::<UserCommand> {
            reactorid: reactorid,
            cmd: cmd,
            deferred: deferred,
            completion: Box::new(completion),
        })
        .unwrap();
        return true;
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
    Complete,     // No message in queue
    InQueue,      // message in queue
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

pub trait MsgDispatcher<UserCommand> {
    /// dispatch msg.
    /// @param msg_size  the decoded message size which is return value of previous call of dispatch. 0 means message having not been decoded.
    /// @return ExpectMsgSize(msgsize) to indicate more read until full msgsize is read then call next dispatch. msgsize==0 means msg size is unknown.
    ///         DropMsgSize(msgsize) to indiciate message is processed already. framework can drop the message after call. then msgsize will be 0 again.
    fn dispatch(
        &mut self,
        buf: &mut [u8],
        msg_size: usize,
        ctx: &mut DispatchContext<UserCommand>,
    ) -> DispatchResult;
}

pub struct DispatchContext<'a, UserCommand> {
    pub sock: &'a mut std::net::TcpStream,
    pub sender: &'a mut MsgSender, // socker sender
    pub cmd_sender: &'a CmdSender<UserCommand>,
}

pub enum DispatchResult {
    Error,                // Error. Exit reading message.
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
    fn try_read<UserCommand>(
        &mut self,
        ctx: &mut DispatchContext<UserCommand>,
        dispatcher: &mut impl MsgDispatcher<UserCommand>,
    ) -> bool {
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
                    } else if errkind == ErrorKind::ConnectionAborted {
                        logmsg!("sock ConnectionAborted : {:?}. close socket", ctx.sock); // closed by remote (windows)
                        return false; // close socket.
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
    type UserCommand = <StreamHandler as Reactor>::UserCommand;

    fn on_new_connection(
        &mut self,
        _conn: &mut std::net::TcpListener,
        _new_conn: &mut std::net::TcpStream,
        _addr: std::net::SocketAddr,
    ) -> Option<Box<dyn Reactor<UserCommand = <StreamHandler as Reactor>::UserCommand>>> {
        Some(Box::new(StreamHandler::default()))
    }
}

//====================================================================================
//            MyReactor
//====================================================================================

pub mod example {
    use super::*;

    #[derive(Copy, Clone)]
    pub struct MsgHeader {
        body_len: u16,
        send_time: i64, // sending timestamp nanos since epoch
    }
    const MSG_HEADER_SIZE: usize = 10;
    const LATENCY_BATCH_SIZE: i32 = 10000;

    pub type MyUserCommand = String;

    pub struct MyReactor {
        dispatcher: EchoAndLatency, // reader & dispacther must be decoupled, in order to avoid double mutable reference of self.
    }

    /// EchoAndLatency reports latency and echo back.
    /// It's owned by MyReactor, which also owns a MsgReader that calls EchoAndLatency::dispatch.
    /// MsgReader & MsgDispatcher must be decoupled.
    /// See issue: https://stackoverflow.com/questions/79015535/could-anybody-optimize-class-design-and-fix-issue-mutable-more-than-once-at-a-t
    struct EchoAndLatency {
        pub parent_listener: ReactorID,
        pub is_client: bool, // default false
        pub max_echo: i32,
        pub count_echo: i32,

        pub latency_batch: i32, // number of messages to report latencies.
        pub last_sent_time: i64,
        pub single_trip_durations: Vec<i64>,
        pub round_trip_durations: Vec<i64>,
    }
    impl MsgDispatcher<MyUserCommand> for EchoAndLatency {
        fn dispatch(
            &mut self,
            buf: &mut [u8],
            msg_size: usize,
            ctx: &mut DispatchContext<MyUserCommand>,
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
            let recvtime = utils::cpu_now_nanos();
            {
                let header: &MsgHeader = utils::bytes_to_ref(&buf[0..MSG_HEADER_SIZE]);
                debug_assert_eq!(header.body_len as usize + MSG_HEADER_SIZE, buf.len());

                if self.last_sent_time > 0 {
                    self.round_trip_durations
                        .push(recvtime - self.last_sent_time);
                    self.single_trip_durations.push(recvtime - header.send_time);
                    dbglog!(
                        "Recv msg sock: {:?} [{}, {}, {}] content: {} <{}>",
                        ctx.sock,
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
            header.send_time = utils::cpu_now_nanos(); // update send_time only

            if self.count_echo < self.max_echo {
                self.last_sent_time = utils::cpu_now_nanos();
                if SendOrQueResult::CloseOrError
                    == ctx.sender.send_or_que(ctx.sock, &buf[..msg_size], || {})
                {
                    return DispatchResult::Error;
                }

                self.count_echo += 1;
            }
            return DispatchResult::DropMsgSize(msg_size);
        }
    }
    impl EchoAndLatency {
        fn report_latencies(&mut self) {
            let fact = 1000;
            // println!(
            //     "RoundTrip Latencies(us) size: {} min      50%       99%     max",
            //     self.round_trip_durations.len()
            // );
            self.round_trip_durations.sort();
            // self.single_trip_durations.sort();
            // let (d, n) = (
            //     &self.single_trip_durations[..],
            //     self.single_trip_durations.len(),
            // );
            // println!(
            //     "SingleTrip        {}       {}      {}      {}",
            //     d[0] / fact,
            //     d[n / 2] / fact,
            //     d[(n as f32 * 0.99) as usize] / fact,
            //     d[n - 2] / fact
            // );

            let (d, n) = (
                &self.round_trip_durations[..],
                self.round_trip_durations.len(),
            );
            println!(
                "RoundTrip time(us) size: {}  min: {}   median: {}    99%: {}   max: {}",
                self.round_trip_durations.len(),
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
                    parent_listener: INVALID_REACTOR_ID,
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
        type UserCommand = MyUserCommand; // mut

        fn on_connected(
            &mut self,
            ctx: &mut ReactorContext<MyUserCommand>,
            listener: ReactorID,
        ) -> bool {
            self.dispatcher.parent_listener = listener;
            if self.dispatcher.is_client {
                logmsg!("client sock: {:?} connected.", ctx.sock);
                let msg_content = b"test msg00";
                let mut buf = vec![0u8; msg_content.len() + MSG_HEADER_SIZE];

                buf[MSG_HEADER_SIZE..(MSG_HEADER_SIZE + msg_content.len())]
                    .copy_from_slice(&msg_content[..]);
                let header: &mut MsgHeader = utils::bytes_to_ref_mut(&mut buf[0..10]);
                header.body_len = msg_content.len() as u16;
                header.send_time = utils::cpu_now_nanos();

                //
                let res = ctx.sender.send_or_que(
                    &mut ctx.sock,
                    &buf[..(msg_content.len() + MSG_HEADER_SIZE)],
                    || {},
                );
                if res == SendOrQueResult::CloseOrError {
                    return false;
                }
                dbglog!("client sent initial msg");
            } else {
                // if it's not client. close parent listener socket.
                ctx.cmd_sender.send_cmd(
                    self.dispatcher.parent_listener,
                    SysCommand::CloseSocket,
                    Deferred::Immediate,
                    |_res| {},
                );

                logmsg!("server sock: {:?} connected.", ctx.sock);
            }
            return true;
        }

        fn on_readable(&mut self, ctx: &mut ReactorContext<MyUserCommand>) -> bool {
            if !ctx.reader.try_read(
                &mut DispatchContext {
                    sock: &mut ctx.sock,
                    sender: &mut ctx.sender,
                    cmd_sender: &ctx.cmd_sender,
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
        fn on_command(&mut self, cmd: MyUserCommand, ctx: &mut ReactorContext<MyUserCommand>) {
            logmsg!("Reactor {} recv cmd: {}", ctx.sockkey.0, cmd);
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    pub fn test_ping_pong_reactor() {
        let addr = "127.0.0.1:12355";
        let mut runtime = ReactRuntime::new();
        runtime
            .start_listen(
                addr,
                DefaultTcpListenerHandler::<example::MyReactor>::new_boxed(),
            )
            .unwrap();
        runtime
            .start_connect(addr, example::MyReactor::new_client(2, 1000))
            .unwrap();
        while runtime.process_events() {}
        assert_eq!(runtime.len(), 0);
    }
    #[test]
    pub fn test_reactors_cmd() {
        let addr = "127.0.0.1:12355";
        let mut runtime = ReactRuntime::new();
        runtime.send_cmd(
            INVALID_REACTOR_ID,
            SysCommand::NewListen(
                addr.to_owned(),
                DefaultTcpListenerHandler::<example::MyReactor>::new_boxed(),
            ),
            Deferred::Immediate,
            |_| {},
        );
        runtime.send_cmd(
            INVALID_REACTOR_ID,
            SysCommand::NewConnect(addr.to_owned(), example::MyReactor::new_client(2, 1000)),
            Deferred::Immediate,
            |_| {},
        );
        while runtime.process_events() {}
        assert_eq!(runtime.len(), 0);
    }
}
