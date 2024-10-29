use reactio::{
    dbglog, logmsg, utils, Deferred, DispatchContext, MessageResult, NewServerReactor, Reactor,
    ReactorID, Result,
};

#[derive(Copy, Clone)]
pub struct MsgHeader {
    pub body_len: u16,  // 2 bytes
    pub send_time: i64, // 8 bytes, sending timestamp nanos since epoch
}
pub struct Msg {
    pub header: MsgHeader,
    pub payload: Vec<u8>,
}
impl Default for Msg {
    fn default() -> Self {
        Self::new()
    }
}
impl Msg {
    pub fn new() -> Self {
        Self {
            header: MsgHeader {
                body_len: 0,
                send_time: 0,
            },
            payload: Vec::new(),
        }
    }
}
const MSG_HEADER_SIZE: usize = 10;
const LATENCY_BATCH_SIZE: i32 = 10000;

pub type MyUserCommand = String;

/// PingpongReactor, working as either client or server, echos back string messages and exit when reaching `max_echo`.
/// It also calculates & print round-trip latencies for every latency_batch number of echos.
pub struct PingpongReactor {
    pub name: String,
    pub parent_listener: ReactorID,
    pub is_client: bool, // default false
    pub max_echo: i32,
    pub count_echo: i32,

    pub latency_batch: i32, // number of messages to report latencies.
    pub last_sent_time: i64,
    pub single_trip_durations: Vec<i64>,
    pub round_trip_durations: Vec<i64>,

    pub last_recv_msg: Msg, // used to verify.
}
impl Default for PingpongReactor {
    fn default() -> Self {
        PingpongReactor::new("".to_owned(), false, i32::MAX, LATENCY_BATCH_SIZE)
    }
}
impl Reactor for PingpongReactor {
    type UserCommand = MyUserCommand;

    fn on_command(
        &mut self,
        cmd: MyUserCommand,
        ctx: &mut DispatchContext<MyUserCommand>,
    ) -> Result<()> {
        logmsg!("Reactorid {} recv cmd: {}", ctx.reactorid, cmd);
        Ok(())
    }

    fn on_connected(
        &mut self,
        ctx: &mut DispatchContext<MyUserCommand>,
        listener: ReactorID,
    ) -> Result<()> {
        self.parent_listener = listener;
        logmsg!("[{}] sock connected: {:?}", self.name, ctx.sock);
        if self.is_client {
            self.send_msg(ctx, "test msg000")?;
        } else {
            // if it's not client. close parent listener socket.
            ctx.cmd_sender
                .send_close(listener, Deferred::Immediate, |_| {})?;
        }
        Ok(())
    }

    fn on_inbound_message(
        &mut self,
        buf: &mut [u8],
        _new_bytes: usize,
        decoded_msg_size: usize,
        ctx: &mut DispatchContext<Self::UserCommand>,
    ) -> Result<MessageResult> {
        let mut msg_size = decoded_msg_size;
        if msg_size == 0 {
            // decode header
            if buf.len() < MSG_HEADER_SIZE {
                return Ok(MessageResult::ExpectMsgSize(0)); // partial header
            }
            let header_bodylen: &u16 = utils::bytes_to_ref(&buf[0..MSG_HEADER_SIZE]);
            msg_size = *header_bodylen as usize + MSG_HEADER_SIZE;

            if msg_size > buf.len() {
                return Ok(MessageResult::ExpectMsgSize(msg_size)); // decoded msg_size but still partial msg. need reading more.
            } // else having read full msg. should NOT return. continue processing.
        }
        debug_assert!(buf.len() >= msg_size); // full msg.
                                              //---- process full message.
        let recvtime = utils::cpu_now_nanos();
        {
            self.last_recv_msg.header.body_len = *utils::bytes_to_ref(&buf[0..MSG_HEADER_SIZE]);
            self.last_recv_msg.header.send_time = *utils::bytes_to_ref(&buf[2..MSG_HEADER_SIZE]);
            debug_assert_eq!(
                self.last_recv_msg.header.body_len as usize + MSG_HEADER_SIZE,
                buf.len()
            );

            if self.last_sent_time > 0 {
                self.round_trip_durations
                    .push(recvtime - self.last_sent_time);
                self.single_trip_durations
                    .push(recvtime - self.last_recv_msg.header.send_time);
                dbglog!(
                    "Recv msg sock: {:?} [{}, {}, {}] content: {} <{}>",
                    ctx.sock,
                    self.last_sent_time,
                    self.last_recv_msg.header.send_time,
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
            ctx.send_msg(&buf[..msg_size])?;

            self.last_recv_msg.payload.clear();
            self.last_recv_msg
                .payload
                .extend_from_slice(&buf[MSG_HEADER_SIZE..]);
            self.count_echo += 1;
            Ok(MessageResult::DropMsgSize(msg_size))
        } else {
            self.last_recv_msg.payload.clear();
            self.last_recv_msg
                .payload
                .extend_from_slice(&buf[MSG_HEADER_SIZE..]);
            Err("Reached max echo. close.".to_owned())
        }
    }
}
impl PingpongReactor {
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
            parent_listener: reactio::INVALID_REACTOR_ID,
            is_client,
            max_echo,
            count_echo: 0,
            latency_batch,
            //
            last_sent_time: 0,
            single_trip_durations: Vec::new(),
            round_trip_durations: Vec::new(),
            last_recv_msg: Msg::new(),
        }
    }

    pub fn new_client(name: String, max_echo: i32, latency_batch: i32) -> Self {
        Self::new(name, true, max_echo, latency_batch)
    }

    /// Used to send initial message.
    pub fn send_msg(&mut self, ctx: &mut DispatchContext<MyUserCommand>, msg: &str) -> Result<()> {
        let mut buf = vec![0u8; msg.len() + MSG_HEADER_SIZE];

        buf[MSG_HEADER_SIZE..(MSG_HEADER_SIZE + msg.len())].copy_from_slice(msg.as_bytes());
        {
            let header_bodylen: &mut u16 = utils::bytes_to_ref_mut(&mut buf[0..MSG_HEADER_SIZE]);
            *header_bodylen = msg.len() as u16;
        }
        {
            let header_sendtime: &mut i64 = utils::bytes_to_ref_mut(&mut buf[2..MSG_HEADER_SIZE]);
            *header_sendtime = utils::cpu_now_nanos();
        }

        ctx.send_msg(&buf[..(msg.len() + MSG_HEADER_SIZE)])?;
        Ok(())
    }
}

/// The parameter used to create a socket when listener socket accepts a connection.
#[derive(Debug, Clone)]
pub struct ServerParam {
    pub name: String,
    pub latency_batch: i32,
}
impl NewServerReactor for PingpongReactor {
    type InitServerParam = ServerParam;
    fn new_server_reactor(count: usize, p: Self::InitServerParam) -> Self {
        PingpongReactor::new(
            format!("{}-{}", p.name, count),
            false,
            i32::MAX,
            p.latency_batch,
        )
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use reactio::{DefaultTcpListenerHandler, ReactRuntime};
    use ServerParam;

    #[test]
    // PingpongReactor is a Reactor to send back any received messages, which could be used to test round-trip TCP time.
    pub fn test_ping_pong_reactor() {
        let addr = "127.0.0.1:12355";
        let recv_buffer_min_size = 1024;
        let mut runtime = ReactRuntime::new();
        let cmd_sender = runtime.get_cmd_sender();
        cmd_sender
            .send_listen(
                addr,
                DefaultTcpListenerHandler::<PingpongReactor>::new(
                    recv_buffer_min_size,
                    ServerParam {
                        name: "server".to_owned(), // parent/listener reactor name. Children names are appended a count number. E.g. "Server-1" for the first connection.
                        latency_batch: 1000, // report round-trip time for each latency_batch samples.
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
                // client PingpongReactor initiate a message. It sends echo back 2 messages before close and latency_batch=1000.
                PingpongReactor::new_client("client".to_owned(), 2, 1000),
                Deferred::Immediate,
                |_| {},
            )
            .unwrap();
        // In non-threaded environment, process_events until there're no reactors, no events, no deferred events.
        let timer = utils::Timer::new_millis(1000);
        while runtime.process_events() {
            if timer.expired() {
                assert!(false, "ERROR: timeout waiting for tests to complete!");
                break;
            }
        }
        assert_eq!(runtime.count_reactors(), 0);
        assert_eq!(runtime.count_deferred_queue(), 0);
    }
}
