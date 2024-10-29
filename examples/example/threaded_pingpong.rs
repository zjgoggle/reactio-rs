use reactio::threaded_reactors::{ReactorUID, ThreadedReactorMgr};
use reactio::{
    logmsg, DefaultTcpListenerHandler, Deferred, DispatchContext, MessageResult, NewServerReactor,
    Reactor, ReactorID, Result,
};
use std::sync::atomic::{self, AtomicI32};
use std::sync::Arc;

use pingpong::PingpongReactor;

use super::*;

/// `ThreadedPingpongReactor` wraps PingpongReactor in multi-threaded environment.
pub struct ThreadedPingpongReactor {
    runtimeid: usize,
    reactormgr: Arc<ThreadedReactorMgr<<PingpongReactor as Reactor>::UserCommand>>,
    stopcounter: Arc<AtomicI32>,
    inner: PingpongReactor,
}
impl ThreadedPingpongReactor {
    pub fn new_client(
        name: String,
        runtimeid: usize,
        reactormgr: Arc<ThreadedReactorMgr<<PingpongReactor as Reactor>::UserCommand>>,
        max_echo: i32,
        latency_batch: i32,
        stopcounter: Arc<AtomicI32>,
    ) -> Self {
        Self {
            runtimeid,
            reactormgr,
            stopcounter,
            inner: PingpongReactor::new_client(name, max_echo, latency_batch),
        }
    }
}
impl Drop for ThreadedPingpongReactor {
    fn drop(&mut self) {
        self.reactormgr.remove_reactor_name(&self.inner.name);
        logmsg!("Dropping reactor: {}", self.inner.name);
        self.stopcounter.fetch_add(1, atomic::Ordering::Relaxed);
    }
}
#[derive(Clone)]
pub struct ThreadedServerParam {
    pub runtimeid: usize,
    pub reactormgr: Arc<ThreadedReactorMgr<<PingpongReactor as Reactor>::UserCommand>>,
    pub stopcounter: Arc<AtomicI32>,
    pub name: String,
    pub latency_batch: i32,
}
impl NewServerReactor for ThreadedPingpongReactor {
    type InitServerParam = ThreadedServerParam;
    fn new_server_reactor(count: usize, p: Self::InitServerParam) -> Self {
        Self {
            runtimeid: p.runtimeid,
            reactormgr: p.reactormgr,
            stopcounter: p.stopcounter,
            inner: PingpongReactor::new(
                format!("{}-{}", p.name, count),
                false,
                i32::MAX,
                p.latency_batch,
            ),
        }
    }
}
impl Reactor for ThreadedPingpongReactor {
    type UserCommand = <PingpongReactor as Reactor>::UserCommand;

    fn on_connected(
        &mut self,
        ctx: &mut DispatchContext<Self::UserCommand>,
        listener: ReactorID,
    ) -> Result<()> {
        self.inner.parent_listener = listener;
        logmsg!("[{}] connected sock: {:?}", self.inner.name, ctx.sock);
        // register <name, uid>
        self.reactormgr.add_reactor_uid(
            self.inner.name.clone(),
            ReactorUID {
                runtimeid: self.runtimeid,
                reactorid: ctx.reactorid,
            },
        )?;
        if self.inner.is_client {
            // send cmd to self to start sending msg to server.
            ctx.cmd_sender.send_user_cmd(
                ctx.reactorid,
                "StartSending".to_owned(),
                Deferred::UtilTime(
                    std::time::SystemTime::now()
                        .checked_add(std::time::Duration::from_millis(10))
                        .expect("Failed att time!"),
                ),
                |_| {},
            )?;
        } else {
            // server
            ctx.cmd_sender
                .send_close(listener, Deferred::Immediate, |_| {})?;
        }
        Ok(())
        // return self.reactor.on_connected(ctx, listener);
    }

    fn on_inbound_message(
        &mut self,
        buf: &mut [u8],
        new_bytes: usize,
        decoded_msg_size: usize,
        ctx: &mut DispatchContext<Self::UserCommand>,
    ) -> Result<MessageResult> {
        self.inner
            .on_inbound_message(buf, new_bytes, decoded_msg_size, ctx)
    }

    fn on_command(
        &mut self,
        cmd: Self::UserCommand,
        ctx: &mut DispatchContext<Self::UserCommand>,
    ) -> Result<()> {
        logmsg!("[{}] **Recv user cmd** {}", &self.inner.name, &cmd);
        if self.inner.is_client {
            //-- test send cmd to server
            let server_uid = self
                .reactormgr
                .find_reactor_uid("server-1")
                .ok_or_else(|| format!("ERROR: Failed to find server name: {}", "server-1"))?;
            let sender_to_server = self
                .reactormgr
                .get_cmd_sender(server_uid.runtimeid)
                .ok_or_else(|| {
                    format!(
                        "ERROR: failed to find sender by runtimeid: {}",
                        server_uid.runtimeid
                    )
                })?;
            sender_to_server.send_user_cmd(
                server_uid.reactorid,
                "TestCmdFromClient".to_owned(),
                Deferred::Immediate,
                |_| {},
            )?;

            //-- send initial msg
            self.inner.send_msg(ctx, "hello world")
        } else {
            Ok(())
        }
        // self.reactor.on_command(cmd, ctx);
    }
}

pub fn create_tcp_listener(
    recv_buffer_min_size: usize,
    param: ThreadedServerParam,
) -> DefaultTcpListenerHandler<ThreadedPingpongReactor> {
    DefaultTcpListenerHandler::<ThreadedPingpongReactor>::new(recv_buffer_min_size, param)
}

#[cfg(test)]
mod test {
    use atomic::AtomicI32;

    use reactio::{utils, CommandCompletion, Deferred};

    use super::*;

    #[test]
    pub fn test_threaded_pingpong() {
        let addr = "127.0.0.1:12355";
        let recv_buffer_min_size = 1024;
        let stopcounter = Arc::new(AtomicI32::new(0)); // each Reactor increases it when exiting.
        let mgr = ThreadedReactorMgr::<String>::new(2); // 2 threads
        let (threadid0, threadid1) = (0, 1);

        // cloned Arc are passed to threads.
        let (amgr, astopcounter) = (Arc::clone(&mgr), Arc::clone(&stopcounter));

        // send a command to mgr to create a listener in threadid0.
        // When the listen socket is ready (command is completed), send another command to connect from threadid1.
        mgr.get_cmd_sender(threadid0)
            .unwrap()
            .send_listen(
                addr,
                create_tcp_listener(
                    recv_buffer_min_size,
                    ThreadedServerParam {
                        runtimeid: threadid0,
                        reactormgr: Arc::clone(&mgr),
                        stopcounter: Arc::clone(&stopcounter),
                        name: "server".to_owned(),
                        latency_batch: 1000,
                    },
                ),
                Deferred::Immediate,
                // OnCompletion, when listen socket is ready, send another command to connect from another thread.
                move |res| {
                    if let CommandCompletion::Error(_) = res {
                        logmsg!("[ERROR] Failed to listen exit!");
                        return;
                    }
                    amgr.get_cmd_sender(threadid1)
                        .unwrap()
                        .send_connect(
                            addr,
                            recv_buffer_min_size,
                            ThreadedPingpongReactor::new_client(
                                "myclient".to_owned(),
                                threadid1,
                                Arc::clone(&amgr),
                                5,
                                1000,
                                Arc::clone(&astopcounter),
                            ),
                            Deferred::Immediate,
                            |res| {
                                if let CommandCompletion::Error(_) = res {
                                    logmsg!("[ERROR] Failed connect!");
                                }
                            },
                        )
                        .unwrap();
                },
            )
            .unwrap();

        // wait for 2 reactors exit
        let timer = utils::Timer::new_millis(1000);
        while stopcounter.load(atomic::Ordering::Relaxed) != 2 {
            timer.sleep_or_expire(10);
            std::thread::yield_now();
            if timer.expired() {
                assert!(false, "ERROR: timeout waiting for reactors to complete");
                break;
            }
        }
    }
}
