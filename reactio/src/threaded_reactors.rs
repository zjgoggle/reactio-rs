use crate::{logmsg, CmdSender, ReactRuntime, ReactorID};
use std::sync::{
    atomic::{self, AtomicBool, AtomicI32},
    Arc, Mutex,
};

/// Threaded reactors are stored in a `ThreadedReactorMgr`. Each thread has a `ReactRuntime`. And each `Reactor` is owned by a `ReactRuntime`.
/// There is also a map<ReactorName, ReactorID>, which is used to find the ReactorID with unique ReactorName and send command to Reactor with ReactorID.
/// * **Note that each reactor must add itsef into reactor_uid_map by add_reactor_uid when on_connected and deregister itself by add_reactor_uid when on_close/on_drop.**
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

        // let mut runtimes : Vec<Arc<Mutex<ReactRuntime<UserCommand>>>> = Vec::new(); // ReactRuntime cannot be accessed across threads.
        // runtimes.resize_with(size, || Arc::new(Mutex::new(ReactRuntime::new())));
        let uninited_senders: Vec<Arc<Mutex<Option<CmdSender<UserCommand>>>>> =
            vec![Arc::new(Mutex::new(None)); size]; // threads set each slot.
        let count_inited_threads = Arc::new(AtomicI32::new(0));
        let startcmd = Arc::new(AtomicBool::new(false));

        for i in 0..size {
            let (stopcmd, startcmd) = (Arc::clone(&me.stopcmd), Arc::clone(&startcmd));
            let uninited_sender = Arc::clone(&uninited_senders[i]);
            let count_inited_threads = Arc::clone(&count_inited_threads);
            // let pruntime = Arc::clone(&runtimes[i]);

            let thread = std::thread::Builder::new()
                .name(format!("ThreadedReactors-{}", i))
                .spawn(move || {
                    let threadid = i;
                    logmsg!("Entered ThreadedReactors-{}", threadid);
                    // let mut guard_runtime = pruntime.lock().unwrap();
                    // let ref mut runtime = *guard_runtime;
                    let mut runtime = ReactRuntime::<UserCommand>::new();
                    {
                        let mut sender_guard = uninited_sender.lock().unwrap();
                        *sender_guard = Some(runtime.get_cmd_sender().clone())
                    }
                    drop(uninited_sender);

                    count_inited_threads.fetch_add(1, atomic::Ordering::Relaxed);
                    drop(count_inited_threads);

                    while startcmd.load(atomic::Ordering::Relaxed) != true {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    drop(startcmd);
                    logmsg!("Start polling events in ThreadedReactors-{}", threadid);
                    while !stopcmd.load(atomic::Ordering::Relaxed) {
                        runtime.process_events();
                        std::thread::yield_now();
                    }
                    logmsg!("Exiting ThreadedReactors-{}", threadid);
                })
                .unwrap();
            me.threads.push(thread);
        }

        logmsg!("Waiting for thread initializations");
        while count_inited_threads.load(atomic::Ordering::Relaxed) < size as i32 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        logmsg!("Recved all thread initializations");
        {
            for (i, sender) in uninited_senders.iter().enumerate() {
                let guard = sender.lock().unwrap();
                match &*guard {
                    Some(s) => me.senders.push(s.clone()),
                    _ => panic!("Thread-{} didn't set CmdSender", i),
                }
            }
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
    // return error when key was already in the map.
    pub fn add_reactor_uid(&self, key: ReactorName, value: ReactorUID) -> Result<(), ()> {
        let mut mapguard = self.reactor_uid_map.lock().unwrap();
        match mapguard.insert(key, value) {
            Some(_) => Err(()),
            _ => Ok(())
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
            listener: crate::ReactorID,
        ) -> bool {
            self.reactor.dispatcher.parent_listener = listener;
            logmsg!(
                "[{}] connected sock: {:?}",
                self.reactor.dispatcher.name,
                ctx.sock
            );
            // register <name, uid>
            self.reactormgr.add_reactor_uid(
                self.reactor.dispatcher.name.clone(),
                super::ReactorUID {
                    runtimeid: self.runtimeid,
                    reactorid: ctx.reactorid,
                },
            ).expect("Duplicate reactor name");
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
                ctx.cmd_sender
                    .send_close(listener, Deferred::Immediate, |_| {})
                    .unwrap();
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
                "[{}] **Recv user cmd** {}",
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
        let mgr = ThreadedReactorMgr::<String>::new(2); // 2 threads
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