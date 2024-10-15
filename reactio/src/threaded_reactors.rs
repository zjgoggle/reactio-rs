use std::sync::{atomic, atomic::AtomicBool, Arc, Mutex};
use crate::{logmsg, CmdSender, ReactRuntime, ReactorID};

/// Threaded reactors are stored in a ThreadedReactorMgr. Each thread has a ReactRuntime. Each Reactor is managed/owned by a ReactRuntime.
/// There is also a map<ReactorName, ReactorID>. So that we can find the ReactorID with unique ReactorName and send command to Reactor with ReactorID.
pub struct ThreadedReactorMgr<UserCommand: 'static> {
    senders: Vec<CmdSender<UserCommand>>,
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
    pub fn new(size: usize) -> Arc<Self> {
        let mut me = Self {
            senders: Vec::new(),
            threads: Vec::new(),
            stopcmd: Arc::new(AtomicBool::new(false)),
            reactor_uid_map: Mutex::new(GlobalReactorUIDMap::new()),
        };
        let startcmd = Arc::new(AtomicBool::new(false));
        let (tx_orig, rx) = std::sync::mpsc::channel::<IDAndSender<UserCommand>>(); // send <idx, CmdSender> from each thread.
        for i in 0..size {
            let (stopcmd, waitstart, tx) = (
                Arc::clone(&me.stopcmd),
                Arc::clone(&startcmd),
                tx_orig.clone(),
            );
            let thread = std::thread::Builder::new()
                .name(format!("ThreadedReactors-{}", i))
                .spawn(move || {
                    let threadid = i;
                    // create runtime
                    let mut runtime = ReactRuntime::<UserCommand>::new();
                    tx.send(IDAndSender {
                        0: threadid,
                        1: runtime.get_cmd_sender().clone(),
                    })
                    .expect("Failed to send cmd_sender to main thread.");

                    while waitstart.load(atomic::Ordering::Relaxed) != true {
                        std::thread::yield_now();
                    }
                    //-- elements of vec senders are created. Now set each cmd_sender.
                    logmsg!("Started ThreadedReactors-{}", threadid);
                    while !stopcmd.load(atomic::Ordering::Relaxed) {
                        runtime.process_events();
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        std::thread::yield_now();
                    }
                    logmsg!("Exiting ThreadedReactors-{}", threadid);
                })
                .unwrap();
            me.threads.push(thread);
        }

        let mut senders = Vec::<IDAndSender<UserCommand>>::new();
        while senders.len() < size {
            match rx.recv_timeout(std::time::Duration::from_millis(10)) {
                Ok(idsender) => {
                    senders.push(idsender);
                }
                _ => {
                    panic!("Failed to recv CmdSender from thread!");
                }
            }
        }
        senders.sort_by(|a, b| a.0.cmp(&b.0));
        for (i, idsender) in senders.into_iter().enumerate() {
            debug_assert_eq!(i, idsender.0);
            me.senders.push(idsender.1);
        }
        startcmd.store(true, atomic::Ordering::Relaxed);
        return Arc::new(me);
    }

    pub fn stop(&self) {
        self.stopcmd.store(true, atomic::Ordering::Relaxed);
    }
    pub fn wait(&mut self) {
        let threads = std::mem::replace(&mut self.threads, Vec::new());
        for t in threads.into_iter() {
            t.join().unwrap();
        }
    }

    fn get_sender(&self, runtimeid: usize) -> Option<&CmdSender<UserCommand>> {
        return self.senders.get(runtimeid);
    }

    //----------------------------- UID Map --------------------------
    pub fn find_reactor_uid(&self, key: &str) -> Option<ReactorUID> {
        let mapguard = self.reactor_uid_map.lock().unwrap();
        match mapguard.get(key) {
            Some(uid) => Some(*uid),
            _ => None,
        }
    }
    /// return None if the value is added; otherwise, value will not be added and return old value.
    pub fn add_reactor_uid(&self, key: ReactorName, value: ReactorUID) -> Option<ReactorUID> {
        let mut mapguard = self.reactor_uid_map.lock().unwrap();
        return mapguard.insert(key, value);
    }

    /// If there was old value update it and return old value; otherwise, return None.
    pub fn update_reactor_uid(&self, key: &str, value: ReactorUID) -> Option<ReactorUID> {
        let mut mapguard = self.reactor_uid_map.lock().unwrap();
        match mapguard.get_mut(key) {
            Some(oldval) => Some(std::mem::replace(oldval, value)),
            _ => None,
        }
    }
    pub fn remove_reactor_name(&self, key: &str) -> Option<ReactorUID> {
        let mut mapguard = self.reactor_uid_map.lock().unwrap();
        return mapguard.remove(key);
    }
    pub fn count_reactors(&self) -> usize {
        let mapguard = self.reactor_uid_map.lock().unwrap();
        return mapguard.len();
    }
}

//====================================================================================
//            MyThreadedReactor
//====================================================================================

pub mod example {
    use crate::threaded_reactors::ThreadedReactorMgr;
    use crate::{example::MyReactor, DefaultTcpListenerHandler, Deferred, Reactor};
    use crate::{logmsg, NewServiceReactor};
    use std::i32;
    use std::sync::atomic::{self, AtomicI32};
    use std::sync::Arc;

    use super::ReactorName;

    pub struct MyThreadedReactor {
        runtimeid: usize,
        reactormgr: Arc<ThreadedReactorMgr<<MyReactor as Reactor>::UserCommand>>,
        stopcounter: Arc<AtomicI32>,
        reactor: MyReactor,
    }
    impl MyThreadedReactor {
        pub fn new_client(
            name: ReactorName,
            runtimeid: usize,
            mgr: Arc<ThreadedReactorMgr<<MyReactor as Reactor>::UserCommand>>,
            max_echo: i32,
            latency_batch: i32,
            stopcounter: Arc<AtomicI32>,
        ) -> Self {
            Self {
                runtimeid: runtimeid,
                reactormgr: mgr,
                stopcounter: stopcounter,
                reactor: MyReactor::new_client(name, max_echo, latency_batch),
            }
        }
    }
    impl Drop for MyThreadedReactor {
        fn drop(&mut self) {
            self.reactormgr
                .remove_reactor_name(&self.reactor.dispatcher.name);
            logmsg!("Dropping reactor: {}", self.reactor.dispatcher.name);
            self.stopcounter.fetch_add(1, atomic::Ordering::Relaxed);
        }
    }
    #[derive(Clone)]
    pub struct ThreadedServiceParam {
        pub runtimeid: usize,
        pub reactormgr: Arc<ThreadedReactorMgr<<MyReactor as Reactor>::UserCommand>>,
        pub stopcounter: Arc<AtomicI32>,
        pub name: String,
        pub latency_batch: i32,
    }
    impl NewServiceReactor for MyThreadedReactor {
        type InitServiceParam = ThreadedServiceParam;
        fn new_service_reactor(p: Self::InitServiceParam) -> Self {
            Self {
                runtimeid: p.runtimeid,
                reactormgr: p.reactormgr,
                stopcounter: p.stopcounter,
                reactor: MyReactor::new(
                    (p.name + "-1").to_owned(),
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
            ctx: &mut crate::ReactorContext<Self::UserCommand>,
            _listener: crate::ReactorID,
        ) -> bool {
            logmsg!(
                "[{}] connected sock: {:?}",
                self.reactor.dispatcher.name,
                ctx.sock
            );
            // register <name, uid>
            let res = self.reactormgr.add_reactor_uid(
                self.reactor.dispatcher.name.clone(),
                super::ReactorUID {
                    runtimeid: self.runtimeid,
                    reactorid: ctx.reactorid,
                },
            );
            assert!(res.is_none());
            if self.reactor.dispatcher.is_client {
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
            }
            return true;
            // return self.reactor.on_connected(ctx, listener);
        }

        fn on_readable(&mut self, ctx: &mut crate::ReactorContext<Self::UserCommand>) -> bool {
            return self.reactor.on_readable(ctx);
        }

        fn on_command(
            &mut self,
            cmd: Self::UserCommand,
            ctx: &mut crate::ReactorContext<Self::UserCommand>,
        ) {
            logmsg!(
                "[{}] ***Recv user cmd*** {}",
                &self.reactor.dispatcher.name,
                &cmd
            );
            if self.reactor.dispatcher.is_client {
                //-- test send cmd to server
                let server_uid = self
                    .reactormgr
                    .find_reactor_uid("server-1")
                    .expect("Failed to find server");
                let sender_to_server = self
                    .reactormgr
                    .senders
                    .get(server_uid.runtimeid)
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
                if !self.reactor.send_msg(ctx, "hello world") {
                    ctx.cmd_sender
                        .send_close(ctx.reactorid, Deferred::Immediate, |_| {})
                        .expect("failed to send close cmd");
                }
            }
            // self.reactor.on_command(cmd, ctx);
        }
    }

    pub fn create_tcp_listener(
        param: ThreadedServiceParam,
    ) -> DefaultTcpListenerHandler<MyThreadedReactor> {
        return DefaultTcpListenerHandler::<MyThreadedReactor>::new(param);
    }
}
#[cfg(test)]
mod test {
    use atomic::AtomicI32;

    use crate::threaded_reactors::example::{create_tcp_listener, ThreadedServiceParam};
    use crate::Deferred;

    use super::*;

    #[test]
    pub fn test_threaded_reactors() {
        let addr = "127.0.0.1:12355";
        let stopcounter = Arc::new(AtomicI32::new(0));
        let mgr = ThreadedReactorMgr::<String>::new(2);
        let (threadid0, threadid1) = (0, 1);
        mgr.get_sender(threadid0)
            .unwrap()
            .send_listen(
                addr,
                create_tcp_listener(ThreadedServiceParam {
                    runtimeid: threadid0,
                    reactormgr: Arc::clone(&mgr),
                    stopcounter: Arc::clone(&stopcounter),
                    name: "server".to_owned(),
                    latency_batch: 1000,
                }),
                Deferred::Immediate,
                |_| {},
            )
            .unwrap();
        mgr.get_sender(threadid1)
            .unwrap()
            .send_connect(
                addr,
                example::MyThreadedReactor::new_client(
                    "myclient".to_owned(),
                    threadid1,
                    Arc::clone(&mgr),
                    5,
                    1000,
                    Arc::clone(&stopcounter),
                ),
                Deferred::Immediate,
                |_| {},
            )
            .unwrap();

        // server reactor doesnot recv disconnection sometimes when removing sleep???
        while stopcounter.load(atomic::Ordering::Relaxed) != 2 {
            std::thread::sleep(std::time::Duration::from_millis(1));
            std::thread::yield_now();
        }
    }
}
