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
    /// * return err to close socket.
    fn on_connected(
        &mut self,
        _ctx: &mut DispatchContext<Self::UserCommand>,
        _listener: ReactorID,
    ) -> Result<()> {
        Ok(()) // accept the connection by default.
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
    ) -> Result<MessageResult>;

    /// ReactRuntime calls it when there's a readable event. `ctx.reader` could be used to read message. See `MsgReader` for usage.
    /// This is a default implementation which uses MsgReader to read all messages then call on_inbound_message to dispatch (default `try_read_fast_read`).
    /// User may override this function to implement other strategies (e.g. `try_read_fast_dispatch``).
    /// * return Err to close socket.
    fn on_readable(&mut self, ctx: &mut ReactorReableContext<Self::UserCommand>) -> Result<()> {
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

    /// ReactRuntime calls it when receiving a command. If no user command is used (e.g. `type UserCommand=();`), user may not need to override it.
    /// return Err to close the socket.
    fn on_command(
        &mut self,
        _cmd: Self::UserCommand,
        ctx: &mut DispatchContext<Self::UserCommand>,
    ) -> Result<()> {
        panic!("Please impl on_command for reactorid: {}", ctx.reactorid);
    }

    /// ReactRuntime calls it when the reactor is removed from poller and before closing the socket.
    /// The Reactor will be destroyed after this call.
    fn on_close(&mut self, _reactorid: ReactorID, _cmd_sender: &CmdSender<Self::UserCommand>) {
        // noops by defaut
    }
}

pub type Result<T> = std::result::Result<T, String>;

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
    pub fn send_msg(&mut self, msg: &[u8]) -> Result<SendOrQueResult> {
        self.sender.send_or_que(self.sock, msg, None)
    }
    pub fn acquire_send(&mut self) -> AutoSendBuffer<'_> {
        let old_buf_size = self.sender.buf.len();
        AutoSendBuffer {
            sender: self.sender,
            sock: self.sock,
            old_buf_size,
        }
    }
}

/// `MessageResult` is returned by `on_inbound_message` to indicate result.
pub enum MessageResult {
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
    ) -> Result<()> {
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
    ) -> Result<()> {
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
    ) -> Result<()> {
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
    ) -> Result<()> {
        self.send_cmd(reactorid, SysCommand::UserCmd(cmd), deferred, completion)
    }

    fn send_cmd(
        &self,
        reactorid: ReactorID,
        cmd: SysCommand<UserCommand>,
        deferred: Deferred,
        completion: impl FnOnce(CommandCompletion) + 'static,
    ) -> Result<()> {
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

// Make a separate struct ReactorMgr because when interating TcpConnectionMgr::events, sessions must be mutable in process_events.
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

pub const INVALID_REACTOR_ID: ReactorID = ReactorID {
    sockslot: HalfUsize::MAX,
    ver: HalfUsize::MAX,
};

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
            return std::io::Result::Ok(reactorid);
        }
        std::io::Result::Ok(INVALID_REACTOR_ID)
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
            if handler
                .on_connected(
                    &mut DispatchContext::from(sockdata, &self.cmd_sender),
                    INVALID_REACTOR_ID,
                )
                .is_ok()
            {
                return std::io::Result::Ok(reactorid);
            }
        }
        self.close_reactor(reactorid);
        std::io::Result::Ok(INVALID_REACTOR_ID)
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
    /// return number if deferred commands in deferred queue which saves deferred commands.
    /// **Note: there's also an internal command queue that is used to receive commands**
    pub fn count_deferred_queue(&self) -> usize {
        self.deferred_data.len()
    }
    /// return number of streams (excluding listeners)
    pub fn count_streams(&self) -> usize {
        self.mgr.count_streams
    }
    /// return total number of received socket events.
    pub fn count_sock_events(&self) -> usize {
        self.accum_sock_events
    }
    /// return total number of received commands.
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
                            let (mut newsock, _) = sock.accept().unwrap();
                            if let Some(new_stream_connection) =
                                handler.on_new_connection(sock, &mut newsock)
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
                            if !ctx.sender.buf.is_empty() {
                                if let Err(err) = ctx.sender.send_queued(&mut ctx.sock) {
                                    logmsg!("{err}  send_queued failed.");
                                    removesock = true;
                                }
                            }
                        }
                        if ev.readable {
                            if let Err(err) = handler.on_readable(&mut ReactorReableContext::from(
                                ctx,
                                &self.mgr.cmd_sender,
                            )) {
                                if !err.is_empty() {
                                    logmsg!("on_readable requested close current_reactorid: {current_reactorid}, sock: {:?}", ctx.sock);
                                }
                                removesock = true;
                            }
                        }
                        if ctx.sender.close_or_error {
                            removesock = true;
                        }
                        // add or remove write interest
                        if !removesock {
                            if !ctx.interested_writable && !ctx.sender.buf.is_empty() {
                                self.mgr
                                    .poller
                                    .modify_with_mode(
                                        &ctx.sock,
                                        Event::all(ev.key),
                                        PollMode::Level,
                                    )
                                    .unwrap();
                                ctx.interested_writable = true;
                            } else if ctx.interested_writable && ctx.sender.buf.is_empty() {
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
                        match newhandler.on_connected(
                            &mut DispatchContext::from(newsockdata, &self.mgr.cmd_sender),
                            current_reactorid,
                        ) {
                            Ok(_) => INVALID_REACTOR_ID, // accept it, don't close it.
                            Err(err) => {
                                if !err.is_empty() {
                                    logmsg!("Reject new connection for listener_reactorid: {}. Reason: {}", current_reactorid, err);
                                }
                                newsockdata.reactorid // close it.
                            }
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
        let mut reactorid_to_close: ReactorID = INVALID_REACTOR_ID;
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
                                let res = (reactor).on_command(
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
                                if let Err(err) = res {
                                    logmsg!(
                                        "on_command requested closing reactorid: {}. {}",
                                        cmddata.reactorid,
                                        err
                                    );
                                    reactorid_to_close = cmddata.reactorid;
                                }
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

        if reactorid_to_close != INVALID_REACTOR_ID {
            self.mgr.close_reactor(reactorid_to_close);
        }
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
    pub pending: FlatStorage<PendingSend>, // Each PendingSend represents a send_msg action.
    first_pending_id: usize, // the id in flat_storage, usize::MAX is invalid. pop from front.
    last_pending_id: usize,  // the id in flat_storage, usize::MAX is invalid. push to back.
    pub bytes_sent: usize,   // total bytes having been sent. buf[0] is bytes_sent+1 byte to send.
    close_or_error: bool,
}
/// Used to save the pending Send action.
pub struct PendingSend {
    next_id: usize, // the id in flat_storage. LinkNode of PendingSend saved in MsgSender::pending.
    startpos: usize, // the first byte of message to sent in buf,
    msgsize: usize,
    completion: Box<dyn FnOnce()>, // notify write completion.
}

/// `SendOrQueResult` is the result of `MsgSender::send_or_que` or `DispatchContext::send_msg`.
#[derive(PartialEq, Eq)]
pub enum SendOrQueResult {
    /// No message in queue
    Complete,
    /// message in queue
    InQueue,
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

    // try send until Err, WOULDBLOCK or Complete.
    // return number of bytes having sent.
    pub fn try_send_all(sock: &mut std::net::TcpStream, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut buf = buf;
        let mut sentbytes = 0;
        loop {
            match sock.write(buf) {
                std::io::Result::Ok(bytes) => {
                    if bytes < buf.len() {
                        buf = &buf[bytes..];
                        sentbytes += bytes; // retry next loop
                    } else {
                        return Ok(sentbytes + bytes); // sent
                    }
                }
                std::io::Result::Err(err) => {
                    let errkind = err.kind();
                    if errkind == ErrorKind::WouldBlock {
                        return Ok(sentbytes); // queued
                    } else if errkind == ErrorKind::ConnectionReset {
                        logmsg!("sock reset : {sock:?}. close socket");
                        return Err(err);
                    // socket closed
                    } else if errkind == ErrorKind::Interrupted {
                        logmsg!("[WARN] sock Interrupted : {sock:?}. retry");
                        return Err(err); // Interrupted is not an error. queue
                    } else {
                        logmsg!("[ERROR]: write on sock {sock:?}, error: {err:?}");
                        return Err(err);
                    }
                }
            }
        }
    }

    /// Send the message or queue it if unabe to send. When there's any messsage is in queue, The `ReactRuntime` will auto send it next time when `process_events`` is called.
    /// * Note that if this function is called with a socket. the same sender should always be used to send socket messages.
    /// * `send_completion` - callback to indicate the message is sent. If there's any error, the socket will be closed and this callback is not called.
    pub fn send_or_que(
        &mut self,
        sock: &mut std::net::TcpStream,
        buf: &[u8],
        send_completion: Option<Box<dyn FnOnce()>>,
    ) -> Result<SendOrQueResult> {
        // let mut buf = buf;
        if buf.is_empty() {
            if let Some(callback) = send_completion {
                (callback)();
            }
            return Ok(SendOrQueResult::Complete);
        }
        if !self.buf.is_empty() {
            self.buf.extend_from_slice(buf);
            self.queue_msg_completion(buf.len(), send_completion);
            return Ok(SendOrQueResult::InQueue);
        }
        // else sendbuf is empty.  try send. queue it if fails.
        debug_assert_eq!(self.bytes_sent, 0);
        debug_assert_eq!(self.first_pending_id, usize::MAX);
        debug_assert_eq!(self.last_pending_id, usize::MAX);
        debug_assert_eq!(self.pending.len(), 0);

        let sentbytes = match MsgSender::try_send_all(sock, buf) {
            Err(err) => {
                return Err(err.to_string());
            }
            Ok(bytes) => bytes,
        };

        if sentbytes == buf.len() {
            if let Some(callback) = send_completion {
                (callback)();
            }
            return Ok(SendOrQueResult::Complete); // sent
        }
        //---- queue the remaining bytes
        self.buf.extend_from_slice(&buf[sentbytes..]);
        self.queue_msg_completion(buf.len() - sentbytes, send_completion);
        Ok(SendOrQueResult::InQueue)
    }

    // call this function after message has been appened to self.buf.
    fn queue_msg_completion(
        &mut self,
        queued_size: usize,
        send_completion: Option<Box<dyn FnOnce()>>,
    ) {
        if let Some(callback) = send_completion {
            // append to last.
            let prev_id = self.last_pending_id;
            self.last_pending_id = self.pending.add(PendingSend {
                next_id: usize::MAX,
                startpos: self.bytes_sent + self.buf.len() - queued_size,
                msgsize: queued_size,
                completion: callback,
            });
            if let Some(prev) = self.pending.get_mut(prev_id) {
                prev.next_id = self.last_pending_id;
            }
            if self.first_pending_id == usize::MAX {
                // add the first one
                self.first_pending_id = self.last_pending_id;
            }
        }
    }

    // This function is called ony ReactRuntime to send messages in queue.
    #[allow(unused_assignments)]
    fn send_queued(&mut self, sock: &mut std::net::TcpStream) -> Result<SendOrQueResult> {
        if self.buf.is_empty() {
            return Ok(SendOrQueResult::Complete);
        }
        let mut sentbytes = 0;
        match sock.write(&self.buf[..]) {
            std::io::Result::Ok(bytes) => {
                sentbytes = bytes;
                if bytes == 0 {
                    self.close_or_error = true;
                    return Err(format!("[ERROR] write sock 0 bytes {sock:?}. close socket"));
                }
            }
            std::io::Result::Err(err) => {
                let errkind = err.kind();
                if errkind == ErrorKind::WouldBlock {
                    return Ok(SendOrQueResult::InQueue); // queued
                } else if errkind == ErrorKind::ConnectionReset {
                    self.close_or_error = true;
                    return Err(format!(
                        "[ERROR] Write sock ConnectionReset {sock:?}. close socket"
                    ));
                // socket closed
                } else if errkind == ErrorKind::Interrupted {
                    logmsg!("[WARN] sock Interrupted : {sock:?}. retry");
                    return Ok(SendOrQueResult::InQueue); // Interrupted is not an error. queue
                } else {
                    self.close_or_error = true;
                    return Err(format!("[ERROR]: write on sock {sock:?}, error: {err:?}"));
                }
            }
        }
        //-- now sent some bytes. pop pending list and notify.
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
        Ok(self.move_buf_front_after_send(sentbytes))
    }
    // return SendOrQueResult::Complete or InQueue
    fn move_buf_front_after_send(&mut self, sentbytes: usize) -> SendOrQueResult {
        //- move front buf
        let len = self.buf.len();
        self.buf.copy_within(sentbytes..len, 0);
        self.buf.resize(len - sentbytes, 0);
        if self.buf.is_empty() {
            // reset members if buf is empty.
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

pub struct AutoSendBuffer<'sender> {
    sender: &'sender mut MsgSender,
    sock: &'sender mut std::net::TcpStream,
    old_buf_size: usize,
}
impl<'sender> AutoSendBuffer<'sender> {
    // clear all unsent bytes.
    pub fn clear(&mut self) {
        self.sender.buf.resize(self.old_buf_size, 0);
    }
    pub fn count_written(&self) -> usize {
        self.sender.buf.len() - self.old_buf_size
    }
    pub fn send(
        &mut self,
        send_completion: Option<Box<dyn FnOnce()>>,
    ) -> std::io::Result<SendOrQueResult> {
        let buf = &self.sender.buf[self.old_buf_size..];
        let buf_len = buf.len();
        if buf.is_empty() {
            if let Some(callback) = send_completion {
                (callback)();
            }
            self.old_buf_size = self.sender.buf.len();
            return Ok(SendOrQueResult::Complete);
        }
        if self.old_buf_size > 0 {
            self.sender.queue_msg_completion(buf.len(), send_completion);
            self.old_buf_size = self.sender.buf.len();
            return Ok(SendOrQueResult::InQueue);
        }

        let sentbytes = match MsgSender::try_send_all(self.sock, buf) {
            Err(err) => {
                self.sender.close_or_error = true;
                self.old_buf_size = self.sender.buf.len();
                return Err(err);
            }
            Ok(bytes) => bytes,
        };

        if sentbytes > 0 {
            self.sender.move_buf_front_after_send(sentbytes);
        }

        if sentbytes == buf_len {
            if let Some(callback) = send_completion {
                (callback)();
            }
            self.old_buf_size = self.sender.buf.len();
            return Ok(SendOrQueResult::Complete); // sent
        }
        //---- queue the remaining bytes
        self.sender
            .queue_msg_completion(self.sender.buf.len(), send_completion);
        self.old_buf_size = self.sender.buf.len();
        Ok(SendOrQueResult::InQueue)
    }
}
impl<'sender> Drop for AutoSendBuffer<'sender> {
    fn drop(&mut self) {
        self.send(None).unwrap(); // send on drop
    }
}
impl<'sender> std::io::Write for AutoSendBuffer<'sender> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.sender.buf.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.send(None)?;
        Ok(())
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
    /// return Err to close socket
    pub fn try_read_fast_dispatch<UserCommand>(
        &mut self,
        ctx: &mut DispatchContext<UserCommand>,
        dispatcher: &mut (impl Reactor<UserCommand = UserCommand> + ?Sized),
    ) -> Result<()> {
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
                        return Err(format!("peer closed sock: {:?}", ctx.sock));
                    }
                    debug_assert!(self.bufsize + new_bytes <= self.recv_buffer.len());

                    self.bufsize += new_bytes;
                    let should_return = self.bufsize < self.recv_buffer.len(); // not full, no need to retry this time.

                    self.try_dispatch_all(new_bytes, ctx, dispatcher)?;
                    if should_return {
                        return Ok(()); // not full, wait for next readable.
                    } else {
                        continue; // try next read.
                    }
                }
                std::io::Result::Err(err) => {
                    let errkind = err.kind();
                    if errkind == ErrorKind::WouldBlock {
                        return Ok(()); // wait for next readable.
                    } else if errkind == ErrorKind::ConnectionReset {
                        return Err(format!("sock reset : {:?}. close socket", ctx.sock));
                    } else if errkind == ErrorKind::Interrupted {
                        logmsg!("[WARN] sock Interrupted : {:?}. retry", ctx.sock);
                        return Ok(()); // Interrupted is not an error.
                    } else if errkind == ErrorKind::ConnectionAborted {
                        return Err(format!(
                            "sock ConnectionAborted : {:?}. close socket",
                            ctx.sock
                        )); // closed by remote (windows)
                    }
                    return Err(format!(
                        "[ERROR]: read on sock {:?}, error: {err:?}",
                        ctx.sock
                    ));
                }
            }
        }
    }

    /// Read until WOULDBLOCK.
    /// return Err to close socket
    pub fn try_read_all<UserCommand>(
        &mut self,
        ctx: &mut DispatchContext<UserCommand>,
    ) -> Result<()> {
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
                        return Err(format!("peer closed sock: {:?}", ctx.sock));
                    }
                    debug_assert!(self.bufsize + new_bytes <= self.recv_buffer.len());

                    self.bufsize += new_bytes;
                    if self.bufsize < self.recv_buffer.len() {
                        // not full, no need to retry this time.
                        return Ok(());
                    }
                }
                std::io::Result::Err(err) => {
                    let errkind = err.kind();
                    if errkind == ErrorKind::WouldBlock {
                        return Ok(()); // wait for next readable.
                    } else if errkind == ErrorKind::ConnectionReset {
                        return Err(format!("sock reset : {:?}. close socket", ctx.sock));
                    // socket closed
                    } else if errkind == ErrorKind::Interrupted {
                        logmsg!("[WARN] sock Interrupted : {:?}. retry", ctx.sock);
                        return Ok(()); // Interrupted is not an error.
                    } else if errkind == ErrorKind::ConnectionAborted {
                        return Err(format!(
                            "sock ConnectionAborted : {:?}. close socket",
                            ctx.sock
                        )); // closed by remote (windows)
                    }
                    return Err(format!(
                        "[ERROR]: read on sock {:?}, error: {err:?}",
                        ctx.sock
                    ));
                }
            } // match
        } // loop
    }

    /// try dispatch all messages in buffer.
    /// return Error if there's any error or close.
    pub fn try_dispatch_all<UserCommand>(
        &mut self,
        new_bytes: usize,
        ctx: &mut DispatchContext<UserCommand>,
        dispatcher: &mut (impl Reactor<UserCommand = UserCommand> + ?Sized),
    ) -> Result<()> {
        let mut new_bytes = new_bytes;
        // loop while: buf_not_empty and ( partial_header or partial_msg )
        while self.startpos < self.bufsize
            && (self.decoded_msgsize == 0 || self.startpos + self.decoded_msgsize <= self.bufsize)
        {
            let res = dispatcher.on_inbound_message(
                &mut self.recv_buffer[self.startpos..self.bufsize],
                new_bytes,
                self.decoded_msgsize,
                ctx,
            )?;
            match res {
                MessageResult::ExpectMsgSize(msgsize) => {
                    if !(msgsize == 0 || msgsize > self.bufsize - self.startpos) {
                        logmsg!( "[WARN] on_inbound_message should NOT expect a msgsize while full message is already received, which may cause recursive call. msgsize:{msgsize:?} recved: {}",
                            self.bufsize - self.startpos);
                        debug_assert!(false, "on_inbound_message expects an already full message.");
                    }
                    self.decoded_msgsize = msgsize; // could be 0 if msg size is unknown.
                    break; // read more in next round.
                }
                MessageResult::DropMsgSize(msgsize) => {
                    assert!(msgsize > 0 && msgsize <= self.bufsize - self.startpos); // drop size should not exceed buffer size.
                    self.startpos += msgsize;
                    self.decoded_msgsize = 0;
                    new_bytes = self.bufsize - self.startpos;
                }
            }
        }
        if self.startpos != 0 {
            // move front
            self.recv_buffer.copy_within(self.startpos..self.bufsize, 0); // don't resize.
            self.bufsize -= self.startpos;
            self.startpos = 0;
        }
        Ok(())
    }

    /// Strategy 2: fast read: read all until WOULDBLOCK, then dispatch all.
    pub fn try_read_fast_read<UserCommand>(
        &mut self,
        ctx: &mut DispatchContext<UserCommand>,
        dispatcher: &mut (impl Reactor<UserCommand = UserCommand> + ?Sized),
    ) -> Result<()> {
        let old_bytes = self.bufsize - self.startpos;
        let res = self.try_read_all(ctx);
        let res2 = self.try_dispatch_all(self.bufsize - self.startpos - old_bytes, ctx, dispatcher);
        res?;
        res2?;
        Ok(())
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
//            SimpleIoReactor, SimpleIoListener, CommandReactor
//====================================================================================

pub type SimpleIoRuntime = ReactRuntime<()>;
pub type SimpleIoReactorContext<'a> = DispatchContext<'a, ()>;
pub type DynIoReactor = dyn Reactor<UserCommand = ()>;

type OnConnectedHandler = dyn FnMut(
    &mut SimpleIoReactorContext<'_>,
    ReactorID, // parent listener reactorid.
) -> Result<()>;

type OnClosedHandler = dyn FnMut(ReactorID, &CmdSender<()>);

type OnSockMsgHandler = dyn FnMut(&mut [u8], &mut SimpleIoReactorContext<'_>) -> Result<usize>;

type CmdHandler<UserCommand> =
    dyn FnMut(UserCommand, &mut DispatchContext<'_, UserCommand>) -> Result<()>;

/// `SimpleIoReactor` doesn't have `UserCommand`. User supplies callback functions to handle inbound socket messages and on_connected/on_close events.
/// On each readable socket event, the MsgReader reads all data and call on_sock_msg_handler to dispatch message.
///
/// **Note that SimpleIoReactor can only be used with SimpleIoRuntime
pub struct SimpleIoReactor {
    on_connected_handler: Option<Box<OnConnectedHandler>>,
    on_closed_handler: Option<Box<OnClosedHandler>>,
    /// msg_handler returns Err to close socket. else, returns Ok(dropMsgSize);
    on_sock_msg_handler: Box<OnSockMsgHandler>,
}
impl SimpleIoReactor {
    pub fn new(
        on_connected_handler: Option<Box<OnConnectedHandler>>,
        on_closed_handler: Option<Box<OnClosedHandler>>,
        on_sock_msg_handler: impl FnMut(&mut [u8], &mut SimpleIoReactorContext<'_>) -> Result<usize>
            + 'static,
    ) -> Self {
        Self {
            on_connected_handler,
            on_closed_handler,
            on_sock_msg_handler: Box::new(on_sock_msg_handler),
        }
    }
    pub fn new_boxed(
        on_connected_handler: Option<Box<OnConnectedHandler>>,
        on_closed_handler: Option<Box<OnClosedHandler>>,
        on_sock_msg_handler: impl FnMut(&mut [u8], &mut SimpleIoReactorContext<'_>) -> Result<usize>
            + 'static,
    ) -> Box<dyn Reactor<UserCommand = ()>> {
        Box::new(Self {
            on_connected_handler,
            on_closed_handler,
            on_sock_msg_handler: Box::new(on_sock_msg_handler),
        })
    }
}
impl Reactor for SimpleIoReactor {
    type UserCommand = ();

    fn on_inbound_message(
        &mut self,
        buf: &mut [u8],
        _new_bytes: usize,
        _decoded_msg_size: usize,
        ctx: &mut DispatchContext<Self::UserCommand>,
    ) -> Result<MessageResult> {
        let drop_msg_size = (self.on_sock_msg_handler)(buf, ctx)?;
        Ok(MessageResult::DropMsgSize(drop_msg_size)) // drop all all messages.
    }
    fn on_connected(
        &mut self,
        ctx: &mut DispatchContext<Self::UserCommand>,
        listener: ReactorID,
    ) -> Result<()> {
        if let Some(ref mut h) = self.on_connected_handler {
            return (h)(ctx, listener);
        }
        Ok(()) // accept the connection by default.
    }
    fn on_close(&mut self, reactorid: ReactorID, cmd_sender: &CmdSender<Self::UserCommand>) {
        if let Some(ref mut h) = self.on_closed_handler {
            (h)(reactorid, cmd_sender)
        }
    }
}

/// `SimpleIoListener` implements TcpListenerHandler. When receiving new connection, it creates SimpleIoReactor using user specified reactor_creator.
pub struct SimpleIoListener {
    count_children: usize,
    reactorid: ReactorID,
    recv_buffer_min_size: usize,
    reactor_creator: Box<dyn FnMut(usize) -> Option<Box<DynIoReactor>>>, // call reactor_creator(children_count) to create SimpleIoReactor
}
impl SimpleIoListener {
    pub fn new(
        recv_buffer_min_size: usize,
        reactor_creator: impl FnMut(usize) -> Option<Box<DynIoReactor>> + 'static,
    ) -> Self {
        Self {
            count_children: 0,
            reactorid: INVALID_REACTOR_ID,
            recv_buffer_min_size,
            reactor_creator: Box::new(reactor_creator),
        }
    }
}
impl TcpListenerHandler for SimpleIoListener {
    type UserCommand = ();

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
    ) -> Option<NewStreamConnection<Self::UserCommand>> {
        self.count_children += 1;
        (self.reactor_creator)(self.count_children).map(|reactor| NewStreamConnection {
            reactor,
            recv_buffer_min_size: self.recv_buffer_min_size,
        })
    }
}

/// `CommandReactor` doesn't have associated sockets, so only process commands by calling cmd_handler.
pub struct CommandReactor<UserCommand> {
    cmd_handler: Box<CmdHandler<UserCommand>>,
}
impl<UserCommand> CommandReactor<UserCommand> {
    pub fn new(
        cmd_handler: impl FnMut(UserCommand, &mut DispatchContext<'_, UserCommand>) -> Result<()>
            + 'static,
    ) -> Self {
        Self {
            cmd_handler: Box::new(cmd_handler),
        }
    }
}
impl<UserCommand> Reactor for CommandReactor<UserCommand> {
    type UserCommand = UserCommand;

    fn on_inbound_message(
        &mut self,
        _buf: &mut [u8],
        _new_bytes: usize,
        _decoded_msg_size: usize,
        _ctx: &mut DispatchContext<Self::UserCommand>,
    ) -> Result<MessageResult> {
        panic!("CommandReactor should not have any associated sockets.");
    }
    fn on_command(
        &mut self,
        cmd: Self::UserCommand,
        ctx: &mut DispatchContext<Self::UserCommand>,
    ) -> Result<()> {
        (self.cmd_handler)(cmd, ctx)
    }
}

#[cfg(test)]
mod test {

    static EMPTY_COMPLETION_FUNC: fn() = || {};
    fn is_empty_function(_fun: &(dyn Fn() + 'static)) -> Option<Box<dyn Fn() + 'static>> {
        // if std::ptr::eq(fun, &EMPTY_COMPLETION_FUNC as &dyn Fn()) {
        // return None;
        // }
        None
        // Some(Box::new((*fun).clone())) // No way to clone Fn()
    }

    #[test]
    pub fn test_compare_function() {
        assert!(is_empty_function(&EMPTY_COMPLETION_FUNC).is_none());
    }
}
