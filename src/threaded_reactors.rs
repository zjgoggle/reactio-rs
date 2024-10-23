use crate::{logmsg, CmdSender, ReactRuntime, ReactorID};
use std::sync::{
    atomic::{self, AtomicBool},
    Arc, Mutex,
};

/// Threaded reactors are stored in a `ThreadedReactorMgr`. Each thread has a `ReactRuntime`. And each `Reactor` is owned by a `ReactRuntime`.
/// There is also a map<ReactorName, ReactorID>, which is used to find the ReactorID with unique ReactorName and send command to Reactor with ReactorID.
/// * **Note that each reactor must add itsef into reactor_uid_map by add_reactor_uid when on_connected and deregister itself by add_reactor_uid when on_close/on_drop.**
pub struct ThreadedReactorMgr<UserCommand: 'static> {
    senders: Vec<IDAndSender<UserCommand>>,
    threads: Vec<std::thread::JoinHandle<()>>,
    stopcmd: Arc<AtomicBool>,
    reactor_uid_map: Mutex<GlobalReactorUIDMap>,
}

type ReactorName = String;

/// ReactorUID consists of a runtimeid and a reactorid.
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ReactorUID {
    pub runtimeid: usize,
    pub reactorid: ReactorID,
}
struct IDAndSender<UserCommand>(usize, CmdSender<UserCommand>);
unsafe impl<UserCommand> Send for IDAndSender<UserCommand> {}

type GlobalReactorUIDMap = std::collections::BTreeMap<ReactorName, ReactorUID>;

impl<UserCommand: 'static> Drop for ThreadedReactorMgr<UserCommand> {
    fn drop(&mut self) {
        self.stop();
        self.wait();
    }
}
impl<UserCommand: 'static> ThreadedReactorMgr<UserCommand> {
    pub fn new(nthreads: usize) -> Arc<Self> {
        let mut me = Self {
            senders: Vec::new(),
            threads: Vec::new(),
            stopcmd: Arc::new(AtomicBool::new(false)),
            reactor_uid_map: Mutex::new(GlobalReactorUIDMap::new()),
        };

        // let mut runtimes : Vec<Arc<Mutex<ReactRuntime<UserCommand>>>> = Vec::new(); // ReactRuntime cannot be accessed across threads.
        // runtimes.resize_with(size, || Arc::new(Mutex::new(ReactRuntime::new())));

        let (tx, rx) = std::sync::mpsc::channel::<IDAndSender<UserCommand>>();

        for threadid in 0..nthreads {
            let (stopcmd, tx) = (Arc::clone(&me.stopcmd), tx.clone());

            let thread = std::thread::Builder::new()
                .name(format!("ThreadedReactors-{}", threadid))
                .spawn(move || {
                    logmsg!("Entered ThreadedReactors-{}", threadid);
                    let mut runtime = ReactRuntime::<UserCommand>::new();
                    tx.send(IDAndSender(threadid, runtime.get_cmd_sender().clone()))
                        .expect("Failed to send in thread");
                    drop(tx);

                    logmsg!("Start polling events in ThreadedReactors-{}", threadid);
                    while !stopcmd.load(atomic::Ordering::Acquire) {
                        runtime.process_events();
                        std::thread::yield_now();
                        // MAYDO: update stats: sock_events, commands, deferred_queue_size, count_read_bytes, count_write_bytes
                    }
                    logmsg!("Exiting ThreadedReactors-{}", threadid);
                })
                .unwrap();
            me.threads.push(thread);
        }

        logmsg!("Waiting for thread initializations");
        let mut unsorted_senders = Vec::new();
        for _ in 0..nthreads {
            let sender = rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .expect("failed to rect msg");
            unsorted_senders.push(sender);
        }
        unsorted_senders.sort_by(|x, y| x.0.cmp(&y.0));
        for (i, e) in unsorted_senders.into_iter().enumerate() {
            debug_assert_eq!(i, e.0);
            me.senders.push(e);
        }
        logmsg!("Recved all thread initializations");
        Arc::new(me)
    }

    pub fn stop(&self) {
        self.stopcmd.store(true, atomic::Ordering::Relaxed);
    }
    pub fn wait(&mut self) {
        let threads = std::mem::take(&mut self.threads); // same as std::mem::replace(&mut self.threads, Vec::new());
        for t in threads.into_iter() {
            t.join().unwrap();
        }
    }

    pub fn get_cmd_sender(&self, runtimeid: usize) -> Option<&CmdSender<UserCommand>> {
        self.senders.get(runtimeid).map(|e| &e.1)
    }

    //----------------------------- UID Map --------------------------
    pub fn find_reactor_uid(&self, key: &str) -> Option<ReactorUID> {
        let mapguard = self.reactor_uid_map.lock().unwrap();
        mapguard.get(key).copied()
    }
    // return Err() when key was already in the map.
    pub fn add_reactor_uid(&self, key: ReactorName, value: ReactorUID) -> Result<(), &'static str> {
        let mut mapguard = self.reactor_uid_map.lock().unwrap();
        match mapguard.insert(key, value) {
            Some(_) => Err("Duplicate ReactorName"),
            _ => Ok(()),
        }
    }

    pub fn remove_reactor_name(&self, key: &str) -> Option<ReactorUID> {
        let mut mapguard = self.reactor_uid_map.lock().unwrap();
        mapguard.remove(key)
    }
    pub fn count_reactors(&self) -> usize {
        let mapguard = self.reactor_uid_map.lock().unwrap();
        mapguard.len()
    }
}

//====================================================================================
//            MyThreadedReactor
//====================================================================================

pub mod example {
    use crate::threaded_reactors::ThreadedReactorMgr;
    use crate::{example::MyReactor, DefaultTcpListenerHandler, Deferred, Reactor};
    use crate::{logmsg, NewServerReactor};
    use std::sync::atomic::{self, AtomicI32};
    use std::sync::Arc;

    use super::ReactorName;

    pub struct MyThreadedReactor {
        runtimeid: usize,
        reactormgr: Arc<ThreadedReactorMgr<<MyReactor as Reactor>::UserCommand>>,
        stopcounter: Arc<AtomicI32>,
        inner: MyReactor,
    }
    impl MyThreadedReactor {
        pub fn new_client(
            name: ReactorName,
            runtimeid: usize,
            reactormgr: Arc<ThreadedReactorMgr<<MyReactor as Reactor>::UserCommand>>,
            max_echo: i32,
            latency_batch: i32,
            stopcounter: Arc<AtomicI32>,
        ) -> Self {
            Self {
                runtimeid,
                reactormgr,
                stopcounter,
                inner: MyReactor::new_client(name, max_echo, latency_batch),
            }
        }
    }
    impl Drop for MyThreadedReactor {
        fn drop(&mut self) {
            self.reactormgr.remove_reactor_name(&self.inner.name);
            logmsg!("Dropping reactor: {}", self.inner.name);
            self.stopcounter.fetch_add(1, atomic::Ordering::Relaxed);
        }
    }
    #[derive(Clone)]
    pub struct ThreadedServerParam {
        pub runtimeid: usize,
        pub reactormgr: Arc<ThreadedReactorMgr<<MyReactor as Reactor>::UserCommand>>,
        pub stopcounter: Arc<AtomicI32>,
        pub name: String,
        pub latency_batch: i32,
    }
    impl NewServerReactor for MyThreadedReactor {
        type InitServerParam = ThreadedServerParam;
        fn new_server_reactor(count: usize, p: Self::InitServerParam) -> Self {
            Self {
                runtimeid: p.runtimeid,
                reactormgr: p.reactormgr,
                stopcounter: p.stopcounter,
                inner: MyReactor::new(
                    format!("{}-{}", p.name, count),
                    false,
                    i32::MAX,
                    p.latency_batch,
                ),
            }
        }
    }
    impl Reactor for MyThreadedReactor {
        type UserCommand = <MyReactor as Reactor>::UserCommand;

        fn on_connected(
            &mut self,
            ctx: &mut crate::DispatchContext<Self::UserCommand>,
            listener: crate::ReactorID,
        ) -> bool {
            self.inner.parent_listener = listener;
            logmsg!("[{}] connected sock: {:?}", self.inner.name, ctx.sock);
            // register <name, uid>
            self.reactormgr
                .add_reactor_uid(
                    self.inner.name.clone(),
                    super::ReactorUID {
                        runtimeid: self.runtimeid,
                        reactorid: ctx.reactorid,
                    },
                )
                .expect("Duplicate reactor name");
            if self.inner.is_client {
                // send cmd to self to start sending msg to server.
                ctx.cmd_sender
                    .send_user_cmd(
                        ctx.reactorid,
                        "StartSending".to_owned(),
                        Deferred::UtilTime(
                            std::time::SystemTime::now()
                                .checked_add(std::time::Duration::from_millis(10))
                                .expect("Failed att time!"),
                        ),
                        |_| {},
                    )
                    .expect("Failed too send user cmd!");
            } else {
                // server
                ctx.cmd_sender
                    .send_close(listener, Deferred::Immediate, |_| {})
                    .unwrap();
            }
            true
            // return self.reactor.on_connected(ctx, listener);
        }

        fn on_inbound_message(
            &mut self,
            buf: &mut [u8],
            new_bytes: usize,
            decoded_msg_size: usize,
            ctx: &mut crate::DispatchContext<Self::UserCommand>,
        ) -> crate::MessageResult {
            self.inner
                .on_inbound_message(buf, new_bytes, decoded_msg_size, ctx)
        }

        fn on_command(
            &mut self,
            cmd: Self::UserCommand,
            ctx: &mut crate::DispatchContext<Self::UserCommand>,
        ) {
            logmsg!("[{}] **Recv user cmd** {}", &self.inner.name, &cmd);
            if self.inner.is_client {
                //-- test send cmd to server
                let server_uid = self
                    .reactormgr
                    .find_reactor_uid("server-1")
                    .expect("Failed to find server");
                let sender_to_server = self
                    .reactormgr
                    .get_cmd_sender(server_uid.runtimeid)
                    .expect("Failed to find sender");
                sender_to_server
                    .send_user_cmd(
                        server_uid.reactorid,
                        "TestCmdFromClient".to_owned(),
                        Deferred::Immediate,
                        |_| {},
                    )
                    .expect("Failed to send cmd to server");

                //-- send initial msg
                if !self.inner.send_msg(ctx, "hello world") {
                    ctx.cmd_sender
                        .send_close(ctx.reactorid, Deferred::Immediate, |_| {})
                        .expect("failed to send close cmd");
                }
            }
            // self.reactor.on_command(cmd, ctx);
        }
    }

    pub fn create_tcp_listener(
        recv_buffer_min_size: usize,
        param: ThreadedServerParam,
    ) -> DefaultTcpListenerHandler<MyThreadedReactor> {
        DefaultTcpListenerHandler::<MyThreadedReactor>::new(recv_buffer_min_size, param)
    }
}

#[cfg(test)]
mod test {
    use atomic::AtomicI32;

    use crate::threaded_reactors::example::{create_tcp_listener, ThreadedServerParam};
    use crate::{CommandCompletion, Deferred};

    use super::*;

    #[test]
    pub fn test_threaded_reactors() {
        let addr = "127.0.0.1:12355";
        let recv_buffer_min_size = 1024;
        let stopcounter = Arc::new(AtomicI32::new(0)); // each Reactor increases it when exiting.
        let mgr = ThreadedReactorMgr::<String>::new(2); // 2 threads
        let (threadid0, threadid1) = (0, 1);

        // cloned Arc are passed to threads.
        let (amgr, astopcounter) = (Arc::clone(&mgr), Arc::clone(&stopcounter));

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
                //  when listen socket is ready, send another command to connect from another thread.
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
                            example::MyThreadedReactor::new_client(
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
        let start = std::time::SystemTime::now();
        while stopcounter.load(atomic::Ordering::Relaxed) != 2 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            std::thread::yield_now();
            if start.elapsed().unwrap() >= std::time::Duration::from_millis(2000) {
                logmsg!("ERROR: timeout waiting for reactors to complete");
                break;
            }
        }
    }
}
