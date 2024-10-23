use crate::dbglog;
use crate::flat_storage::FlatStorage;
use crate::logmsg; // import one macro per line. macros are exported at root of crate instead of mod level.
use crate::utils;
use core::panic;
use polling::{Event, Events, PollMode, Poller};
use std::io::{ErrorKind, Read, Write};
use std::time::Duration;
use std::{marker::PhantomData, net::TcpStream};

//====================================================================================
//            Reactor, Poller
//====================================================================================

/// A `Reactor` is assigned a unique ReactorID when adding to a ReactRuntime, and is able to receive socket messsages (via reader) and commands.
/// `process_events` of a ReactRuntime instance should be periodically called in a dedicated thread.
/// Besides socket communication, Sending command is the only thread-safe way to communicate with a Reactor.
/// A Reactor could send socket messages (via sender), or send commands (via cmd_sender) to another Reactor with specific ReactorID.
/// A Reactor is destroyed when the socket is closed.
pub trait Reactor {
    type UserCommand;

    /// ReactRuntime calls it when connection is established.
    /// * `ctx`  - The context the used for reactor to send socket message or command.
    ///   * **Note that ctx.cmd_sener can only send command to a reactor that belongs to the same ReactRuntime.**
    /// * `listener` - The listener ID when the reactor is created by a listener socket; otherwise, it's INVALID_REACTOR_ID.
    /// * return true to accept the connection; false to close the connection.
    fn on_connected(
        &mut self,
        _ctx: &mut DispatchContext<Self::UserCommand>,
        _listener: ReactorID,
    ) -> bool {
        true // accept the connection by default.
    }

    /// It's called by in on_readable() when either decoded_msg_size==0 (meaning message size is unknown) or decoded_msg_size <= buf.len() (meaning a full message is read).
    ///
    /// * `buf`  - The buffer containing all the received bytes
    /// * `new_bytes` - Number of new received bytes, which are also bytes that are not processed. If previously on_inbound_message returned DropMsgSize. new_bytes is the remaining bytes.
    /// * `decoded_msg_size` -  the decoded message size which is return value of previous call of on_inbound_message. 0 means message having not been decoded.
    /// * return ExpectMsgSize(msgsize) to indicate more read until full msgsize is read then call next dispatch. msgsize==0 indicates msg size is unknown.
    ///         DropMsgSize(msgsize) to indiciate message is processed already. framework can drop the message after call. then msgsize will be 0 again.
    /// * **Note that when calling on_inbound_message(decoded_msg_size) -> ExpectMsgSize(expect_msg_size), if expect_msg_size!=0, it should be always > msg_size.**
    fn on_inbound_message(
        &mut self,
        buf: &mut [u8],
        new_bytes: usize,
        decoded_msg_size: usize,
        ctx: &mut DispatchContext<Self::UserCommand>,
    ) -> MessageResult;

    /// ReactRuntime calls it when there's a readable event. `ctx.reader` could be used to read message. See `MsgReader` for usage.
    /// This is a default implementation which uses MsgReader to read all messages then call on_inbound_message to dispatch (default `try_read_fast_read`).
    /// User may override this function to implement other strategies (e.g. `try_read_fast_dispatch``).
    /// * return false to close socket.
    fn on_readable(&mut self, ctx: &mut ReactorReableContext<Self::UserCommand>) -> bool {
        ctx.reader.try_read_fast_read(
            &mut DispatchContext {
                reactorid: ctx.reactorid,
                sock: ctx.sock,
                sender: ctx.sender,
                cmd_sender: ctx.cmd_sender,
            },
            self,
        )
    }

    /// ReactRuntime calls it when receiving a command.
    fn on_command(
        &mut self,
        _cmd: Self::UserCommand,
        ctx: &mut DispatchContext<Self::UserCommand>,
    ) {
        panic!("Please impl on_command for reactorid: {}", ctx.reactorid);
    }

    /// ReactRuntime calls it when the reactor is removed from poller and before closing the socket.
    /// The Reactor will be destroyed after this call.
    fn on_close(&mut self, _reactorid: ReactorID, _cmd_sender: &CmdSender<Self::UserCommand>) {
        // noops by defaut
    }
}

/// `DispatchContext` contains all info that could be used to dispatch command/message to reactors.
pub struct DispatchContext<'a, UserCommand> {
    pub reactorid: ReactorID,
    pub sock: &'a mut std::net::TcpStream,
    pub sender: &'a mut MsgSender, // socker sender
    pub cmd_sender: &'a CmdSender<UserCommand>,
}
impl<'a, UserCommand> DispatchContext<'a, UserCommand> {
    fn from(data: &'a mut SockData, cmd_sender: &'a CmdSender<UserCommand>) -> Self {
        Self {
            reactorid: data.reactorid,
            sock: &mut data.sock,
            sender: &mut data.sender,
            cmd_sender,
        }
    }
}

/// `MessageResult` is returned by `on_inbound_message` to indicate result.
pub enum MessageResult {
    /// Error or close. Exit message reading and close socket.
    Close,
    /// Having received a partial message. Expecting more, indicating decoded/expected message size. If it's non-zero, `on_inbound_message`` will not be called until full message is read;
    ///     if it's 0, meaning the message size is unknown, `on_inbound_message` will be called everytime when there are any bytes read.
    /// * **Note that when calling on_inbound_message(decoded_msg_size) -> ExpectMsgSize(expect_msg_size), if expect_msg_size!=0, it should be always > msg_size.**
    ExpectMsgSize(usize),
    /// Full message has been processed, indicating bytes to drop. Next call `on_inbound_message` with argument decoded_msgsize=0 and all rest bytes will be treated as unprocessed.
    DropMsgSize(usize),
}

/// `Deferred` is used to indicate a command to be executed immidately or in a deferred time.
pub enum Deferred {
    Immediate,
    UtilTime(std::time::SystemTime),
}
/// `CommandCompletion` is used as an argument of command completion callback.
pub enum CommandCompletion {
    /// Command execution is completed.
    Completed(ReactorID),
    /// Command was not executed due to error.
    Error(String),
}

/// CmdSender is owned by a ReactRuntime. Users send commands to a reactor with specific ReactorID.
/// * **Note that CmdSender can only send command to a reactor that belongs to the same ReactRuntime.**
pub struct CmdSender<UserCommand>(std::sync::mpsc::Sender<CmdData<UserCommand>>);
/// `CmdSender` is a `Send` so that is can be passed through threads.
unsafe impl<UserCommand> Send for CmdSender<UserCommand> {}

impl<UserCommand> Clone for CmdSender<UserCommand> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<UserCommand> CmdSender<UserCommand> {
    /// Send a command to create a socket to connect to remote IP:Port. The reactor will receive socket messages once connected.
    /// # Arguments
    /// * `remote_addr` -  Remote address in format IP:Port.
    /// * `reactor`     -  The reactor to be add to ReactRuntime to handle the socket messages.
    /// * `deferred`    -  Indicate the command to be executed immediately or in a deferred time.
    /// * `completion`  -  Callback to indicate if the command has been executed or failed (e.g. reactorid doesn't exist).
    pub fn send_connect<AReactor: Reactor<UserCommand = UserCommand> + 'static>(
        &self,
        remote_addr: &str,
        recv_buffer_min_size: usize,
        reactor: AReactor,
        deferred: Deferred,
        completion: impl FnOnce(CommandCompletion) + 'static,
    ) -> Result<(), String> {
        self.send_cmd(
            INVALID_REACTOR_ID,
            SysCommand::NewConnect(
                Box::new(reactor),
                remote_addr.to_owned(),
                recv_buffer_min_size,
            ),
            deferred,
            completion,
        )
    }
    /// Send a command to create a listen socket at IP:Port. The reactor will listen on the socket.
    pub fn send_listen<AReactor: TcpListenerHandler<UserCommand = UserCommand> + 'static>(
        &self,
        local_addr: &str,
        reactor: AReactor,
        deferred: Deferred,
        completion: impl FnOnce(CommandCompletion) + 'static,
    ) -> Result<(), String> {
        self.send_cmd(
            INVALID_REACTOR_ID,
            SysCommand::NewListen(Box::new(reactor), local_addr.to_owned()),
            deferred,
            completion,
        )
    }

    /// Send a command to close a reactor and it's socket.
    pub fn send_close(
        &self,
        reactorid: ReactorID,
        deferred: Deferred,
        completion: impl FnOnce(CommandCompletion) + 'static,
    ) -> Result<(), String> {
        self.send_cmd(reactorid, SysCommand::CloseSocket, deferred, completion)
    }

    /// Send a UserCommand to a reactor with specified `reactorid`.
    /// The existance of reactorid is not check in this function.
    /// When `process_events` is called and the deferred time becomes current,
    /// `reactorid` is checked before passing the cmd to reactor.
    pub fn send_user_cmd(
        &self,
        reactorid: ReactorID,
        cmd: UserCommand,
        deferred: Deferred,
        completion: impl FnOnce(CommandCompletion) + 'static,
    ) -> Result<(), String> {
        self.send_cmd(reactorid, SysCommand::UserCmd(cmd), deferred, completion)
    }

    fn send_cmd(
        &self,
        reactorid: ReactorID,
        cmd: SysCommand<UserCommand>,
        deferred: Deferred,
        completion: impl FnOnce(CommandCompletion) + 'static,
    ) -> Result<(), String> {
        // check NewConnect/NewListen when reactor == INVALID.
        match &cmd {
            SysCommand::NewListen(_, _) | SysCommand::NewConnect(_, _, _) => {
                if reactorid != INVALID_REACTOR_ID {
                    return Err(
                        "reactorid msut be INVALID_REACTOR_ID if NewConnect/NewListen".to_owned(),
                    );
                }
            }
            SysCommand::UserCmd(_) => {
                if reactorid == INVALID_REACTOR_ID {
                    return Err("UserCmd must has a valid reactorid.".to_owned());
                }
                // the reactor id must exist, which is checked when runtime processes the command.
            }
            _ => {}
        }
        if self
            .0
            .send(CmdData::<UserCommand> {
                reactorid,
                cmd,
                deferred,
                completion: Box::new(completion),
            })
            .is_err()
        {
            return Err("Failed to send. Receiver disconnected.".to_owned());
        }
        Ok(())
    }
}

/// `ReactorReableContext` is a helper for a reactor to send/recv socket message, or send command.
pub struct ReactorReableContext<'a, UserCommand> {
    pub reactorid: ReactorID,                   // current reactorid.
    pub sock: &'a mut std::net::TcpStream,      // associated socket.
    pub sender: &'a mut MsgSender,              // helper to send socket message.
    pub reader: &'a mut MsgReader,              // helper to read socket message.
    pub cmd_sender: &'a CmdSender<UserCommand>, // helper to send command.
}
/// `TcpListenerHandler` handles incoming connections on a listening socket.
/// Similar to `Reactor`, it's destroyed when listening socket is closed.
pub trait TcpListenerHandler {
    type UserCommand;

    /// called when the listen socket starts listeing.
    fn on_start_listen(&mut self, reactorid: ReactorID, cmd_sender: &CmdSender<Self::UserCommand>);

    //// return (Reactor, recv_buffer_min_size) or None to close the new connection.
    fn on_new_connection(
        &mut self,
        sock: &mut std::net::TcpListener,
        new_sock: &mut std::net::TcpStream,
        addr: std::net::SocketAddr,
    ) -> Option<NewStreamConnection<Self::UserCommand>>;

    fn on_close(&mut self, _reactorid: ReactorID, _cmd_sender: &CmdSender<Self::UserCommand>) {}
}
pub struct NewStreamConnection<UserCommand> {
    pub reactor: Box<dyn Reactor<UserCommand = UserCommand>>,
    pub recv_buffer_min_size: usize,
}

/// ReactRuntime manages & owns Reactors which receive/send socket data or command.
/// A ReactRuntime has a command queue, deferred command queue and a collection of reactors.
/// Each reactor is assigned a ReactorID when adding to ReactRuntime. Users send command to a reactor
/// with a specific ReactorID that belongs to the ReactRuntime. The command could be immediate or deferred for a time.
/// E.g, on close of a reactor, it could send a command to the ReactRuntime to reconnect in future.
///
/// Communication between ReactRuntimes are via sending command also, which is the only thread-safe way.
/// * **Note that `process_events` of a ReactRuntime instance should be periodically called in a dedicated thread.**
pub struct ReactRuntime<UserCommand> {
    mgr: ReactorMgr<UserCommand>,
    deferred_data: FlatStorage<CmdData<UserCommand>>,
    deferred_heap: Vec<DeferredKey>, // min heap of (scheduled_time_nanos, Cmd_index_in_deferred_data)
    sock_events: Events, // decoupled events and connections to avoid double mutable refererence.
    accum_sock_events: usize, // all events counting since last reset.
    accum_commands: usize, // commands received since last reset.
}

#[derive(Copy, Clone)]
struct DeferredKey {
    millis: i64,
    data: usize,
}
impl DeferredKey {
    fn get_key(&self) -> i64 {
        self.millis
    }
}

// push the last element to a min heap. sift up
fn min_heap_push(v: &mut [DeferredKey]) {
    let mut k = v.len() - 1; // last element
    if k == 0 {
        return;
    }
    let mut parent = (k - 1) / 2;
    while k > 0 && v[k].get_key() < v[parent].get_key() {
        v.swap(k, parent);
        k = parent;
        parent = (k - 1) / 2;
    }
}
// pop the first element to end. sift down.
fn min_heap_pop(v: &mut [DeferredKey]) {
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
    count_streams: usize, // TcpStreams only (excluding TcpListener)
    cmd_recv: std::sync::mpsc::Receiver<CmdData<UserCommand>>,
    cmd_sender: CmdSender<UserCommand>,
}

enum TcpSocketHandler<UserCommand> {
    ListenerType(
        ReactorID,
        std::net::TcpListener,
        Box<dyn TcpListenerHandler<UserCommand = UserCommand>>,
    ), // <sock, handler, key_in_flat_storage>
    StreamType(SockData, Box<dyn Reactor<UserCommand = UserCommand>>),
}
struct SockData {
    pub reactorid: ReactorID,
    pub sock: std::net::TcpStream,
    pub sender: MsgSender,
    pub reader: MsgReader,
    interested_writable: bool,
}
impl<'a, UserCommand> ReactorReableContext<'a, UserCommand> {
    fn from(data: &'a mut SockData, cmd_sender: &'a CmdSender<UserCommand>) -> Self {
        Self {
            reactorid: data.reactorid,
            sock: &mut data.sock,
            sender: &mut data.sender,
            reader: &mut data.reader,
            cmd_sender,
        }
    }
}

#[cfg(target_pointer_width = "64")]
type HalfUsize = u32;
#[cfg(target_pointer_width = "32")]
type HalfUSize = u16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReactorID {
    sockslot: HalfUsize, // must be half of usize
    ver: HalfUsize,      // must be half of usize
}

impl ReactorID {
    /// convert to epoll event key
    pub fn to_usize(&self) -> usize {
        let halfbits = std::mem::size_of::<usize>() * 8 / 2;
        ((self.ver as usize) << halfbits) | (self.sockslot as usize)
    }
    /// convert from epoll event key
    pub fn from_usize(val: usize) -> Self {
        let halfbits = std::mem::size_of::<usize>() * 8 / 2;
        Self {
            sockslot: val as HalfUsize,
            ver: (val >> halfbits) as HalfUsize,
        }
    }
}
impl std::fmt::Display for ReactorID {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.sockslot, self.ver)
    }
}

pub const INVALID_REACTOR_ID: ReactorID = ReactorID {
    sockslot: HalfUsize::MAX,
    ver: HalfUsize::MAX,
};

impl<UserCommand> ReactorMgr<UserCommand> {
    fn new() -> Self {
        let (cmd_sender, cmd_recv) = std::sync::mpsc::channel::<CmdData<UserCommand>>();
        Self {
            socket_handlers: FlatStorage::new(),
            poller: Poller::new().unwrap(),
            count_streams: 0,
            cmd_sender: CmdSender(cmd_sender),
            cmd_recv,
        }
    }
    /// \return number of all managed socks.
    fn len(&self) -> usize {
        self.socket_handlers.len()
    }
    fn add_stream(
        &mut self,
        recv_buffer_min_size: usize,
        sock: std::net::TcpStream,
        handler: Box<dyn Reactor<UserCommand = UserCommand>>,
    ) -> ReactorID {
        let key = self.socket_handlers.add(TcpSocketHandler::StreamType(
            SockData {
                reactorid: INVALID_REACTOR_ID,
                sock,
                sender: MsgSender::new(),
                reader: MsgReader::new(recv_buffer_min_size),
                interested_writable: false,
            },
            handler,
        ));
        let reactorid = ReactorID {
            sockslot: key as HalfUsize,
            ver: self.socket_handlers.len() as HalfUsize,
        };
        self.count_streams += 1;
        if let TcpSocketHandler::StreamType(sockdata, ref mut _handler) =
            self.socket_handlers.get_mut(key).unwrap()
        {
            sockdata.reactorid = reactorid;
            unsafe {
                self.poller
                    .add_with_mode(
                        &sockdata.sock,
                        Event::readable(reactorid.to_usize()),
                        PollMode::Level,
                    )
                    .unwrap();
            }
            logmsg!(
                "Added TcpStream reactorid: {}, sock: {:?}",
                sockdata.reactorid,
                sockdata.sock
            );
            return sockdata.reactorid;
        }
        panic!("ERROR! Failed to get new added sockdata!");
    }

    fn add_listener(
        &mut self,
        sock: std::net::TcpListener,
        handler: Box<dyn TcpListenerHandler<UserCommand = UserCommand>>,
    ) -> ReactorID {
        let key = self.socket_handlers.add(TcpSocketHandler::ListenerType(
            INVALID_REACTOR_ID,
            sock,
            handler,
        ));
        let reactorid = ReactorID {
            sockslot: key as HalfUsize,
            ver: self.socket_handlers.len() as HalfUsize,
        };
        if let TcpSocketHandler::ListenerType(areactorid, ref sock, _) =
            self.socket_handlers.get_mut(key).unwrap()
        {
            *areactorid = reactorid;
            // must read exaustively.
            unsafe {
                self.poller
                    .add_with_mode(sock, Event::readable(reactorid.to_usize()), PollMode::Level)
                    .unwrap();
            }
            logmsg!(
                "Added TcpListener reactorid: {}, sock: {:?}",
                reactorid,
                sock
            );
        }
        reactorid
    }

    /// Close & remove socket/reactor.
    /// * return true if key exists; false when key doesn't exist or it's in process of polling.
    fn close_reactor(&mut self, reactorid: ReactorID) -> bool {
        if let Some(sockhandler) = self.socket_handlers.remove(reactorid.sockslot as usize) {
            match sockhandler {
                TcpSocketHandler::StreamType(sockdata, mut reactor) => {
                    debug_assert_eq!(reactorid, sockdata.reactorid);
                    logmsg!(
                        "removing reactorid: {}, sock: {:?}, pending_send_bytes: {}",
                        reactorid,
                        sockdata.sock,
                        sockdata.sender.buf.len()
                    );
                    self.count_streams -= 1;
                    self.poller.delete(&sockdata.sock).unwrap();
                    (reactor).on_close(reactorid, &self.cmd_sender);
                }
                TcpSocketHandler::ListenerType(areactorid, sock, mut reactor) => {
                    debug_assert_eq!(reactorid, areactorid);
                    logmsg!("removing reactorid: {}, sock: {:?}", reactorid, sock);
                    self.poller.delete(&sock).unwrap();
                    (reactor).on_close(reactorid, &self.cmd_sender);
                }
            }
            return true;
        }
        false
    }

    /// * local_addr - ip:port. e.g. "127.0.0.1:8000"
    fn start_listen(
        &mut self,
        local_addr: &str,
        handler: Box<dyn TcpListenerHandler<UserCommand = UserCommand>>,
    ) -> std::io::Result<ReactorID> {
        let socket = std::net::TcpListener::bind(local_addr)?;
        socket.set_nonblocking(true)?;
        let reactorid = self.add_listener(socket, handler);
        if let TcpSocketHandler::ListenerType(_, _, ref mut handler) = self
            .socket_handlers
            .get_mut(reactorid.sockslot as usize)
            .unwrap()
        {
            handler.on_start_listen(reactorid, &self.cmd_sender);
            return Result::Ok(reactorid);
        }
        Result::Ok(INVALID_REACTOR_ID)
    }

    fn start_connect(
        &mut self,
        remote_addr: &str,
        recv_buffer_min_size: usize,
        handler: Box<dyn Reactor<UserCommand = UserCommand>>,
    ) -> std::io::Result<ReactorID> {
        let socket = TcpStream::connect(remote_addr)?;
        socket.set_nonblocking(true)?; // FIXME: use blocking socket and each nonblocking read and blocking write.
        let reactorid = self.add_stream(recv_buffer_min_size, socket, handler);
        if let TcpSocketHandler::StreamType(ref mut sockdata, ref mut handler) = self
            .socket_handlers
            .get_mut(reactorid.sockslot as usize)
            .unwrap()
        {
            if handler.on_connected(
                &mut DispatchContext::from(sockdata, &self.cmd_sender),
                INVALID_REACTOR_ID,
            ) {
                return Result::Ok(reactorid);
            }
        }
        self.close_reactor(reactorid);
        Result::Ok(INVALID_REACTOR_ID)
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
            deferred_data: FlatStorage::new(),
            deferred_heap: Vec::new(),
            sock_events: Events::new(),
            accum_sock_events: 0,
            accum_commands: 0,
        }
    }
    /// `process_events` should be periodically called to process socket messages, commands and deferred commands.
    /// - return true if the runtime has any of reactors, commands or deferred commands;
    /// - return false when there's no reactor/socket or command, then this runtime could be destroyed.
    pub fn process_events(&mut self) -> bool {
        self.process_events_with(1, 32)
    }
    pub fn process_events_with(&mut self, sock_timeout_millis: u64, max_commands: usize) -> bool {
        let sock_events = self.process_sock_events(sock_timeout_millis);
        self.accum_sock_events += sock_events;
        self.process_deferred_queue();
        let cmds = self.process_command_queue(max_commands);
        self.accum_commands += cmds;
        sock_events > 0 || cmds > 0 || !self.deferred_heap.is_empty() || self.mgr.len() > 0
    }
    /// Count listeners, Reactors
    pub fn count_reactors(&self) -> usize {
        self.mgr.len()
    }
    /// return number if deferred commands in deferred queue (not command queue)
    pub fn count_deferred_queue(&self) -> usize {
        self.deferred_data.len()
    }
    /// return number of streams (non-listener reactors)
    pub fn count_streams(&self) -> usize {
        self.mgr.count_streams
    }
    pub fn count_sock_events(&self) -> usize {
        self.accum_sock_events
    }
    pub fn count_received_commands(&self) -> usize {
        self.accum_commands
    }
    /// Get the `CmdSender` managed by this ReactRuntime. It's used to send command to reactors in this ReactRuntime.
    pub fn get_cmd_sender(&self) -> &CmdSender<UserCommand> {
        &self.mgr.cmd_sender
    }

    /// return number of socket events processed.
    pub fn process_sock_events(&mut self, timeout_millis: u64) -> usize {
        self.sock_events.clear();
        self.mgr
            .poller
            .wait(
                &mut self.sock_events,
                Some(Duration::from_millis(timeout_millis)),
            )
            .unwrap(); // None duration means forever

        for ev in self.sock_events.iter() {
            let mut removesock = false;
            let current_reactorid = ReactorID::from_usize(ev.key);
            let mut new_connection_to_add = None;
            if let Some(sockhandler) = self
                .mgr
                .socket_handlers
                .get_mut(current_reactorid.sockslot as usize)
            {
                match sockhandler {
                    TcpSocketHandler::ListenerType(reactorid, sock, handler) => {
                        debug_assert_eq!(current_reactorid, *reactorid);
                        if ev.readable {
                            let (mut newsock, addr) = sock.accept().unwrap();
                            if let Some(new_stream_connection) =
                                handler.on_new_connection(sock, &mut newsock, addr)
                            {
                                newsock.set_nonblocking(true).unwrap();
                                new_connection_to_add = Some((
                                    newsock,
                                    new_stream_connection.reactor,
                                    new_stream_connection.recv_buffer_min_size,
                                ));
                            }
                            // else newsock will auto destroy
                        }
                        if ev.writable {
                            logmsg!("[ERROR] writable listener sock!");
                            removesock = true;
                        }
                    }
                    TcpSocketHandler::StreamType(ref mut ctx, ref mut handler) => {
                        debug_assert_eq!(current_reactorid, ctx.reactorid);
                        if ev.writable {
                            if !ctx.interested_writable {
                                dbglog!("WARN: unsolicited writable sock: {:?}", ctx.sock);
                            }
                            ctx.interested_writable = true; // in case unsolicited event.
                            if !ctx.sender.pending.is_empty()
                                && SendOrQueResult::CloseOrError
                                    == ctx.sender.send_queued(&mut ctx.sock)
                            {
                                removesock = true;
                            }
                        }
                        if ev.readable {
                            removesock = !handler.on_readable(&mut ReactorReableContext::from(
                                ctx,
                                &self.mgr.cmd_sender,
                            ));
                        }
                        if ctx.sender.close_or_error {
                            removesock = true;
                        }
                        // add or remove write interest
                        if !removesock {
                            if !ctx.interested_writable && !ctx.sender.pending.is_empty() {
                                self.mgr
                                    .poller
                                    .modify_with_mode(
                                        &ctx.sock,
                                        Event::all(ev.key),
                                        PollMode::Level,
                                    )
                                    .unwrap();
                                ctx.interested_writable = true;
                            } else if ctx.interested_writable && ctx.sender.pending.is_empty() {
                                self.mgr
                                    .poller
                                    .modify_with_mode(
                                        &ctx.sock,
                                        Event::readable(ev.key),
                                        PollMode::Level,
                                    )
                                    .unwrap();
                                ctx.interested_writable = false;
                            }
                        }
                    }
                }
            } else {
                dbglog!("[ERROR] socket key has been removed {}!", current_reactorid);
                continue;
            }

            if let Some((newsock, newhandler, recv_buffer_min_size)) = new_connection_to_add {
                let newreactorid_to_close = {
                    let newreactorid =
                        self.mgr
                            .add_stream(recv_buffer_min_size, newsock, newhandler);
                    if let TcpSocketHandler::StreamType(ref mut newsockdata, ref mut newhandler) =
                        self.mgr
                            .socket_handlers
                            .get_mut(newreactorid.sockslot as usize)
                            .unwrap()
                    {
                        if newhandler.on_connected(
                            &mut DispatchContext::from(newsockdata, &self.mgr.cmd_sender),
                            current_reactorid,
                        ) {
                            INVALID_REACTOR_ID // accept it, don't close it.
                        } else {
                            newsockdata.reactorid // close it.
                        }
                    } else {
                        panic!("Failed to find new added stream!");
                    }
                };
                if newreactorid_to_close != INVALID_REACTOR_ID {
                    self.mgr.close_reactor(newreactorid_to_close);
                }

                continue;
            }

            if removesock {
                self.mgr.close_reactor(current_reactorid);
                continue; // ignore error events.
            }
            if ev.is_err().unwrap_or(false) {
                logmsg!("WARN: socket error key: {}", current_reactorid);
                removesock = true;
            }
            if ev.is_interrupt() {
                logmsg!("WARN: socket interrupt key: {}", current_reactorid);
                removesock = true;
            }
            if removesock {
                self.mgr.close_reactor(current_reactorid);
            }
        }

        self.sock_events.len()
    }

    /// return number of command procesed
    pub fn process_command_queue(&mut self, max_commands: usize) -> usize {
        let mut count_cmd = 0usize;
        // max number of commands to process for each call of process_command_queue. So to have chances to process socket/deferred events.
        for _ in 0..max_commands {
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
                    if !ReactRuntime::<UserCommand>::is_deferred_current(millis) {
                        // if beyond half millis tolerance
                        let key = self.deferred_data.add(cmddata);
                        self.deferred_heap.push(DeferredKey { millis, data: key });
                        min_heap_push(&mut self.deferred_heap);
                        continue; // continue loop recv
                    }
                }
            }
            self.execute_immediate_cmd(cmddata);
        } // loop
        count_cmd
    }

    /// return timeout/executed commands
    pub fn process_deferred_queue(&mut self) -> usize {
        let mut cmds = 0;
        while !self.deferred_heap.is_empty()
            && ReactRuntime::<UserCommand>::is_deferred_current(self.deferred_heap[0].millis)
        {
            let key = self.deferred_heap[0].data;
            min_heap_pop(&mut self.deferred_heap);
            self.deferred_heap.pop();
            cmds += 1;
            if let Some(cmddata) = self.deferred_data.remove(key) {
                self.execute_immediate_cmd(cmddata);
            } else {
                panic!("No deferred CommandData with key: {}", key);
            }
        }
        cmds
    }

    //----------------------------- private -----------------------------------------------

    fn is_deferred_current(millis: i64) -> bool {
        let now_nanos = utils::now_nanos();
        millis * 1000000 + 5 * 100000 <= now_nanos
    }

    fn execute_immediate_cmd(&mut self, cmddata: CmdData<UserCommand>) {
        match cmddata.cmd {
            SysCommand::NewConnect(reactor, remote_addr, recv_buffer_min_size) => {
                match self
                    .mgr
                    .start_connect(&remote_addr, recv_buffer_min_size, reactor)
                {
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
            SysCommand::NewListen(reactor, local_addr) => {
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
                if self.mgr.close_reactor(cmddata.reactorid) {
                    (cmddata.completion)(CommandCompletion::Completed(cmddata.reactorid));
                } else {
                    (cmddata.completion)(CommandCompletion::Error(format!(
                        "Failed to remove non existing socket with reactorid: {}",
                        cmddata.reactorid
                    )));
                }
            }
            SysCommand::UserCmd(usercmd) => {
                if cmddata.reactorid == INVALID_REACTOR_ID {
                    panic!("UserCommand must be executed on a reactor!");
                } else if let Some(handler) = self
                    .mgr
                    .socket_handlers
                    .get_mut(cmddata.reactorid.sockslot as usize)
                {
                    match handler {
                        TcpSocketHandler::ListenerType(reactorid, _, _) => {
                            (cmddata.completion)(CommandCompletion::Error(format!(
                                    "Listener cannot receive user command. cmd reactorid: {}, reactorid: {}",
                                    cmddata.reactorid, *reactorid
                                )));
                        }
                        TcpSocketHandler::StreamType(ctx, reactor) => {
                            if cmddata.reactorid != ctx.reactorid {
                                (cmddata.completion)(CommandCompletion::Error(format!(
                                        "Failed to execute user command with wrong cmd reactorid: {}, found: {}",
                                        cmddata.reactorid , ctx.reactorid
                                    )));
                            } else {
                                (reactor).on_command(
                                    usercmd,
                                    &mut DispatchContext {
                                        reactorid: cmddata.reactorid,
                                        sock: &mut ctx.sock,
                                        sender: &mut ctx.sender,
                                        cmd_sender: &self.mgr.cmd_sender,
                                    },
                                );
                                (cmddata.completion)(CommandCompletion::Completed(
                                    cmddata.reactorid,
                                ));
                            }
                        }
                    }
                } else {
                    (cmddata.completion)(CommandCompletion::Error(format!(
                        "Failed to execute user command on non existing socket with reactorid: {}",
                        cmddata.reactorid
                    )));
                }
            }
        } // match cmd
    }
}

//====================================================================================
//            SysCommand to Reactor
//====================================================================================

enum SysCommand<UserCommand> {
    //-- system commands are processed by Runtime, Reactor will not receive them.
    NewConnect(
        Box<dyn Reactor<UserCommand = UserCommand>>,
        String, // connect to remote IP:Port
        usize,  // min_recev_buffer_size
    ),
    NewListen(
        Box<dyn TcpListenerHandler<UserCommand = UserCommand>>,
        String, // connect to remote IP:Port
    ), // listen on IP:Port
    CloseSocket,
    UserCmd(UserCommand),
}

struct CmdData<UserCommand> {
    reactorid: ReactorID,
    cmd: SysCommand<UserCommand>,
    deferred: Deferred,
    completion: Box<dyn FnOnce(CommandCompletion)>,
}
unsafe impl<UserCommand> Send for CmdData<UserCommand> {}

//====================================================================================
//            MsgSender
//====================================================================================

/// MsgSender is a per-socket object. It tries sending msg on a non-blocking socket. if sending fails due to WOULDBLOCK,
/// the unsent bytes are saved and register a Write insterest in poller, so that
/// the remaining data will be scheduled to send on next Writeable event.
///
/// If a reactor should choose either MsgSender or socket to send messages.
/// Mixed using of both may cause out-of-order messages.
pub struct MsgSender {
    pub buf: Vec<u8>,
    pub pending: FlatStorage<PendingSend>,
    first_pending_id: usize, // the id in flat_storage, usize::MAX is invalid
    last_pending_id: usize,  // the id in flat_storage, usize::MAX is invalid
    pub bytes_sent: usize,   // total bytes having been sent. buf[0] is bytes_sent+1 byte to send.
    close_or_error: bool,
}
/// Used to save the pending Send action.
pub struct PendingSend {
    next_id: usize,  // the id in flat_storage
    startpos: usize, // the first byte of message to sent in buf,
    msgsize: usize,
    completion: Box<dyn FnOnce()>, // notify write completion.
}

/// `SendOrQueResult` is the result of `MsgSender::send_or_que`.
#[derive(PartialEq, Eq)]
pub enum SendOrQueResult {
    /// No message in queue
    Complete,
    /// message in queue
    InQueue,
    /// close socket.
    CloseOrError,
}
impl Default for MsgSender {
    fn default() -> Self {
        Self::new()
    }
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
    /// Send the message or queue it if unabe to send. When there's any messsage is in queue, The `ReactRuntime` will auto send it next time when `process_events`` is called.
    /// * Note that if this function is called with a socket. the same sender should always be used to send socket messages.
    /// * `send_completion` - callback to indicate the message is sent. If there's any error, the socket will be closed and this callback is not called.
    pub fn send_or_que(
        &mut self,
        sock: &mut std::net::TcpStream,
        buf: &[u8],
        send_completion: impl Fn() + 'static,
    ) -> SendOrQueResult {
        let mut buf = buf;
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
        loop {
            match sock.write(buf) {
                std::io::Result::Ok(bytes) => {
                    if bytes == 0 {
                        logmsg!("[ERROR] sock 0 bytes {sock:?}. close socket");
                        self.close_or_error = true;
                        return SendOrQueResult::CloseOrError;
                    } else if bytes < buf.len() {
                        buf = &buf[bytes..];
                        sentbytes += bytes; // retry next loop
                    } else {
                        send_completion();
                        return SendOrQueResult::Complete; // sent
                    }
                }
                std::io::Result::Err(err) => {
                    let errkind = err.kind();
                    if errkind == ErrorKind::WouldBlock {
                        break; // queued
                    } else if errkind == ErrorKind::ConnectionReset {
                        logmsg!("sock reset : {sock:?}. close socket");
                        self.close_or_error = true;
                        return SendOrQueResult::CloseOrError;
                    // socket closed
                    } else if errkind == ErrorKind::Interrupted {
                        logmsg!("[WARN] sock Interrupted : {sock:?}. retry");
                        break; // Interrupted is not an error. queue
                    } else {
                        logmsg!("[ERROR]: write on sock {sock:?}, error: {err:?}");
                        self.close_or_error = true;
                        return SendOrQueResult::CloseOrError;
                    }
                }
            }
        }
        //---- queue the remaining bytes
        self.queue_msg(&buf[sentbytes..], send_completion);
        SendOrQueResult::InQueue
    }

    fn queue_msg(&mut self, buf: &[u8], send_completion: impl Fn() + 'static) {
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

    // This function is called ony ReactRuntime to send messages in queue.
    #[allow(unused_assignments)]
    fn send_queued(&mut self, sock: &mut std::net::TcpStream) -> SendOrQueResult {
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
            SendOrQueResult::Complete
        } else {
            self.bytes_sent += sentbytes;
            SendOrQueResult::InQueue
        }
    }
}

//====================================================================================
//            MsgReader
//====================================================================================

/// `MsgReader` is a per-socket helper to read socket messages. It auto handles partial/multiple messages in recv buffer.  
/// On Readable event, call MsgReader::try_read_fast_dispatch/try_read_fast_read to read messages from a sock into a recv_buffer and calls dispatcher::on_inbound_message to dispatch message.
pub struct MsgReader {
    recv_buffer: Vec<u8>,
    min_reserve: usize,     // the min reserved buffer size before each read
    startpos: usize,        // msg start position or first effective byte.
    bufsize: usize,         // count from buffer[0] to last read byte.
    decoded_msgsize: usize, // decoded msg size from MessageResult::ExpectMsgSize,
}

impl MsgReader {
    pub fn new(min_reserved_bytes: usize) -> Self {
        Self {
            // dispatcher : dispatch,
            recv_buffer: vec![0u8; min_reserved_bytes],
            min_reserve: min_reserved_bytes,
            startpos: 0,
            bufsize: 0,
            decoded_msgsize: 0,
        }
    }

    /// Strategy 1: fast dispatch: dispatch on each read of about min_reserve bytes.
    pub fn try_read_fast_dispatch<UserCommand>(
        &mut self,
        ctx: &mut DispatchContext<UserCommand>,
        dispatcher: &mut (impl Reactor<UserCommand = UserCommand> + ?Sized),
    ) -> bool {
        loop {
            debug_assert!(
                self.decoded_msgsize == 0 || self.decoded_msgsize > self.bufsize - self.startpos
            ); // msg size is not decoded, or have not received expected msg size.

            if self.bufsize + self.min_reserve > self.recv_buffer.len() {
                self.recv_buffer.resize(
                    std::cmp::max(self.bufsize + self.min_reserve, self.recv_buffer.len() * 2),
                    0,
                ); // double buffer size
            }
            match ctx.sock.read(&mut self.recv_buffer[self.bufsize..]) {
                std::io::Result::Ok(new_bytes) => {
                    if new_bytes == 0 {
                        logmsg!("peer closed sock: {:?}", ctx.sock);
                        return false;
                    }
                    debug_assert!(self.bufsize + new_bytes <= self.recv_buffer.len());

                    self.bufsize += new_bytes;
                    let should_return = self.bufsize < self.recv_buffer.len(); // not full, no need to retry this time.

                    if !self.try_dispatch_all(new_bytes, ctx, dispatcher) {
                        return false;
                    }
                    if should_return {
                        return true; // not full, wait for next readable.
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

    /// Read until WOULDBLOCK.
    /// return false if there's any error or peer closed; return true, if WouldBlock.
    pub fn try_read_all<UserCommand>(&mut self, ctx: &mut DispatchContext<UserCommand>) -> bool {
        loop {
            if self.bufsize + self.min_reserve > self.recv_buffer.len() {
                self.recv_buffer.resize(
                    std::cmp::max(self.bufsize + self.min_reserve, self.recv_buffer.len() * 2),
                    0,
                ); // double buffer size
            }
            match ctx.sock.read(&mut self.recv_buffer[self.bufsize..]) {
                std::io::Result::Ok(new_bytes) => {
                    if new_bytes == 0 {
                        logmsg!("peer closed sock: {:?}", ctx.sock);
                        return false;
                    }
                    debug_assert!(self.bufsize + new_bytes <= self.recv_buffer.len());

                    self.bufsize += new_bytes;
                    if self.bufsize < self.recv_buffer.len() {
                        // not full, no need to retry this time.
                        return true;
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
            } // match
        } // loop
    }

    /// try dispatch all messages in buffer.
    /// return false if there's any error
    pub fn try_dispatch_all<UserCommand>(
        &mut self,
        new_bytes: usize,
        ctx: &mut DispatchContext<UserCommand>,
        dispatcher: &mut (impl Reactor<UserCommand = UserCommand> + ?Sized),
    ) -> bool {
        let mut new_bytes = new_bytes;
        // loop while: buf_not_empty and ( partial_header or partial_msg )
        while self.startpos < self.bufsize
            && (self.decoded_msgsize == 0 || self.startpos + self.decoded_msgsize <= self.bufsize)
        {
            match dispatcher.on_inbound_message(
                &mut self.recv_buffer[self.startpos..self.bufsize],
                new_bytes,
                self.decoded_msgsize,
                ctx,
            ) {
                MessageResult::ExpectMsgSize(msgsize) => {
                    if !(msgsize == 0 || msgsize > self.bufsize - self.startpos) {
                        logmsg!( "[WARN] on_inbound_message should NOT expect a msgsize while full message is already received, which may cause recursive call. msgsize:{msgsize:?} recved: {}",
                            self.bufsize - self.startpos);
                        debug_assert!(false, "on_inbound_message expects an already full message.");
                    }
                    self.decoded_msgsize = msgsize; // could be 0 if msg size is unknown.
                    new_bytes = 0; // all has been processed.
                }
                MessageResult::DropMsgSize(msgsize) => {
                    assert!(msgsize > 0 && msgsize <= self.bufsize - self.startpos); // drop size should not exceed buffer size.
                    self.startpos += msgsize;
                    self.decoded_msgsize = 0;
                    new_bytes = self.bufsize - self.startpos;
                }
                MessageResult::Close => {
                    logmsg!("Dispatch requested closing sock: {:?}. ", ctx.sock);
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
        true
    }

    /// Strategy 2: fast read: read all until WOULDBLOCK, then dispatch all.
    pub fn try_read_fast_read<UserCommand>(
        &mut self,
        ctx: &mut DispatchContext<UserCommand>,
        dispatcher: &mut (impl Reactor<UserCommand = UserCommand> + ?Sized),
    ) -> bool {
        let old_bytes = self.bufsize - self.startpos;
        let ok = self.try_read_all(ctx);
        let ok2 = self.try_dispatch_all(self.bufsize - self.startpos - old_bytes, ctx, dispatcher);
        ok && ok2
    }
}

//====================================================================================
//         Default TcpListenerHandler
//====================================================================================

/// NewServerReactor is used by TcpListenerHandler to create a new reactor when accept a new socket.
pub trait NewServerReactor: Reactor {
    /// The parameter to create NewServerReactor
    type InitServerParam: Clone;

    /// * count - It's the number of Reactors that DefaultTcpListenerHandler has created, starting from 1.
    fn new_server_reactor(count: usize, param: Self::InitServerParam) -> Self;
}
pub struct DefaultTcpListenerHandler<NewReactor: NewServerReactor + 'static> {
    pub reactorid: ReactorID,
    count_children: usize,
    server_param: <NewReactor as NewServerReactor>::InitServerParam,
    recv_buffer_min_size: usize,
    _phantom: PhantomData<NewReactor>,
}

//---------------------- !NewServerReactor ----------------

impl<NewReactor: NewServerReactor + 'static> DefaultTcpListenerHandler<NewReactor> {
    pub fn new(
        recv_buffer_min_size: usize,
        param: <NewReactor as NewServerReactor>::InitServerParam,
    ) -> Self {
        Self {
            reactorid: INVALID_REACTOR_ID,
            count_children: 0,
            server_param: param,
            recv_buffer_min_size,
            _phantom: PhantomData,
        }
    }
}

impl<NewReactor: NewServerReactor + 'static> TcpListenerHandler
    for DefaultTcpListenerHandler<NewReactor>
{
    type UserCommand = <NewReactor as Reactor>::UserCommand;

    fn on_start_listen(
        &mut self,
        reactorid: ReactorID,
        _cmd_sender: &CmdSender<Self::UserCommand>,
    ) {
        self.reactorid = reactorid;
    }
    fn on_new_connection(
        &mut self,
        _conn: &mut std::net::TcpListener,
        _new_conn: &mut std::net::TcpStream,
        _addr: std::net::SocketAddr,
    ) -> Option<NewStreamConnection<Self::UserCommand>> {
        self.count_children += 1;
        Some(NewStreamConnection {
            reactor: Box::new(NewReactor::new_server_reactor(
                self.count_children,
                self.server_param.clone(),
            )),
            recv_buffer_min_size: self.recv_buffer_min_size,
        })
    }
}
//====================================================================================
//            example: MyReactor
//====================================================================================

pub mod example {
    use super::*;

    #[derive(Copy, Clone)]
    pub struct MsgHeader {
        body_len: u16,  // 2 bytes
        send_time: i64, // 8 bytes, sending timestamp nanos since epoch
    }
    const MSG_HEADER_SIZE: usize = 10;
    const LATENCY_BATCH_SIZE: i32 = 10000;

    pub type MyUserCommand = String;

    /// MyReactor, working as either client or server, echos back string messages and exit when reaching `max_echo`.
    /// It also calculates & print round-trip latencies for every latency_batch number of echos.
    pub struct MyReactor {
        pub name: String,
        pub parent_listener: ReactorID,
        pub is_client: bool, // default false
        pub max_echo: i32,
        pub count_echo: i32,

        pub latency_batch: i32, // number of messages to report latencies.
        pub last_sent_time: i64,
        pub single_trip_durations: Vec<i64>,
        pub round_trip_durations: Vec<i64>,
    }
    impl Default for MyReactor {
        fn default() -> Self {
            MyReactor::new("".to_owned(), false, i32::MAX, LATENCY_BATCH_SIZE)
        }
    }
    impl Reactor for MyReactor {
        type UserCommand = MyUserCommand;

        fn on_command(&mut self, cmd: MyUserCommand, ctx: &mut DispatchContext<MyUserCommand>) {
            logmsg!("Reactorid {} recv cmd: {}", ctx.reactorid, cmd);
        }

        fn on_connected(
            &mut self,
            ctx: &mut DispatchContext<MyUserCommand>,
            listener: ReactorID,
        ) -> bool {
            self.parent_listener = listener;
            logmsg!("[{}] sock connected: {:?}", self.name, ctx.sock);
            if self.is_client {
                self.send_msg(ctx, "test msg000");
            } else {
                // if it's not client. close parent listener socket.
                ctx.cmd_sender
                    .send_close(listener, Deferred::Immediate, |_| {})
                    .unwrap();
            }
            true
        }

        fn on_inbound_message(
            &mut self,
            buf: &mut [u8],
            _new_bytes: usize,
            decoded_msg_size: usize,
            ctx: &mut DispatchContext<Self::UserCommand>,
        ) -> MessageResult {
            let mut msg_size = decoded_msg_size;
            if msg_size == 0 {
                // decode header
                if buf.len() < MSG_HEADER_SIZE {
                    return MessageResult::ExpectMsgSize(0); // partial header
                }
                let header_bodylen: &u16 = utils::bytes_to_ref(&buf[0..MSG_HEADER_SIZE]);
                msg_size = *header_bodylen as usize + MSG_HEADER_SIZE;

                if msg_size > buf.len() {
                    return MessageResult::ExpectMsgSize(msg_size); // decoded msg_size but still partial msg. need reading more.
                } // else having read full msg. should NOT return. continue processing.
            }
            debug_assert!(buf.len() >= msg_size); // full msg.
                                                  //---- process full message.
            let recvtime = utils::cpu_now_nanos();
            {
                let header_bodylen: &u16 = utils::bytes_to_ref(&buf[0..MSG_HEADER_SIZE]);
                let header_sendtime: &i64 = utils::bytes_to_ref(&buf[2..MSG_HEADER_SIZE]);
                debug_assert_eq!(*header_bodylen as usize + MSG_HEADER_SIZE, buf.len());

                if self.last_sent_time > 0 {
                    self.round_trip_durations
                        .push(recvtime - self.last_sent_time);
                    self.single_trip_durations.push(recvtime - *header_sendtime);
                    dbglog!(
                        "Recv msg sock: {:?} [{}, {}, {}] content: {} <{}>",
                        ctx.sock,
                        self.last_sent_time,
                        *header_sendtime,
                        recvtime,
                        buf.len(),
                        std::str::from_utf8(&buf[MSG_HEADER_SIZE..]).unwrap()
                    );
                }
            }
            if self.round_trip_durations.len() as i32 == self.latency_batch {
                self.report_latencies();
            }
            let header_sendtime: &mut i64 = utils::bytes_to_ref_mut(&mut buf[2..MSG_HEADER_SIZE]);
            *header_sendtime = utils::cpu_now_nanos(); // update send_time only

            if self.count_echo < self.max_echo {
                self.last_sent_time = utils::cpu_now_nanos();
                if SendOrQueResult::CloseOrError
                    == ctx.sender.send_or_que(ctx.sock, &buf[..msg_size], || {})
                {
                    return MessageResult::Close;
                }

                self.count_echo += 1;
                MessageResult::DropMsgSize(msg_size)
            } else {
                MessageResult::Close
            }
        }
    }
    impl MyReactor {
        fn report_latencies(&mut self) {
            let fact = 1000;
            self.round_trip_durations.sort();

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

        pub fn new(name: String, is_client: bool, max_echo: i32, latency_batch: i32) -> Self {
            Self {
                name,
                parent_listener: INVALID_REACTOR_ID,
                is_client,
                max_echo,
                count_echo: 0,
                latency_batch,
                //
                last_sent_time: 0,
                single_trip_durations: Vec::new(),
                round_trip_durations: Vec::new(),
            }
        }

        pub fn new_client(name: String, max_echo: i32, latency_batch: i32) -> Self {
            Self::new(name, true, max_echo, latency_batch)
        }

        /// Used to send initial message.
        pub fn send_msg(&mut self, ctx: &mut DispatchContext<MyUserCommand>, msg: &str) -> bool {
            let mut buf = vec![0u8; msg.len() + MSG_HEADER_SIZE];

            buf[MSG_HEADER_SIZE..(MSG_HEADER_SIZE + msg.len())].copy_from_slice(msg.as_bytes());
            {
                let header_bodylen: &mut u16 =
                    utils::bytes_to_ref_mut(&mut buf[0..MSG_HEADER_SIZE]);
                *header_bodylen = msg.len() as u16;
            }
            {
                let header_sendtime: &mut i64 =
                    utils::bytes_to_ref_mut(&mut buf[2..MSG_HEADER_SIZE]);
                *header_sendtime = utils::cpu_now_nanos();
            }

            //
            let res =
                ctx.sender
                    .send_or_que(ctx.sock, &buf[..(msg.len() + MSG_HEADER_SIZE)], || {});
            res != SendOrQueResult::CloseOrError
        }
    }

    /// The parameter used to create a socket when listener socket accepts a connection.
    #[derive(Debug, Clone)]
    pub struct ServerParam {
        pub name: String,
        pub latency_batch: i32,
    }
    impl NewServerReactor for MyReactor {
        type InitServerParam = ServerParam;
        fn new_server_reactor(count: usize, p: Self::InitServerParam) -> Self {
            MyReactor::new(
                format!("{}-{}", p.name, count),
                false,
                i32::MAX,
                p.latency_batch,
            )
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use example::ServerParam;

    #[test]
    pub fn test_reactors_cmd() {
        let addr = "127.0.0.1:12355";
        let recv_buffer_min_size = 1024;
        let mut runtime = ReactRuntime::new();
        let cmd_sender = runtime.get_cmd_sender();
        cmd_sender
            .send_listen(
                addr,
                DefaultTcpListenerHandler::<example::MyReactor>::new(
                    recv_buffer_min_size,
                    ServerParam {
                        name: "server".to_owned(),
                        latency_batch: 1000,
                    },
                ),
                Deferred::Immediate,
                |_| {},
            )
            .unwrap();
        cmd_sender
            .send_connect(
                addr,
                recv_buffer_min_size,
                example::MyReactor::new_client("client".to_owned(), 2, 1000),
                Deferred::Immediate,
                |_| {},
            )
            .unwrap();
        // In single threaded environment, process_events until there're no reactors, no events, no deferred events.
        while runtime.process_events() {}
        assert_eq!(runtime.count_reactors(), 0);
    }
}
