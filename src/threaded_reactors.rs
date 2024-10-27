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
    reactor_uid_map: Mutex<GlobalReactorUIDMap>, // MAYDO: remove mutex. each thread has a UIDMap. broadcast ReactorName removal/deletion.
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

    /// Get `CmdSender` for the runtimeid/threadid. We use CmdSender to send commands to that ReactRuntime running in thread with threadid.
    pub fn get_cmd_sender(&self, runtimeid: usize) -> Option<&CmdSender<UserCommand>> {
        self.senders.get(runtimeid).map(|e| &e.1)
    }

    //----------------------------- UID Map --------------------------

    /// Find ReactorUID(runtimeid, reactorid) with the ReactorName.
    pub fn find_reactor_uid(&self, key: &str) -> Option<ReactorUID> {
        let mapguard = self.reactor_uid_map.lock().unwrap();
        mapguard.get(key).copied()
    }
    // return Err() when key was already in the map.
    pub fn add_reactor_uid(&self, key: ReactorName, value: ReactorUID) -> Result<(), String> {
        let mut mapguard = self.reactor_uid_map.lock().unwrap();
        match mapguard.insert(key, value) {
            Some(_) => Err("Duplicate ReactorName".to_owned()),
            _ => Ok(()),
        }
    }

    pub fn remove_reactor_name(&self, key: &str) -> Option<ReactorUID> {
        let mut mapguard = self.reactor_uid_map.lock().unwrap();
        mapguard.remove(key)
    }
    /// count reactors that managed by UID Map (or added by `add_reactor_uid`)
    pub fn count_reactors(&self) -> usize {
        let mapguard = self.reactor_uid_map.lock().unwrap();
        mapguard.len()
    }
}
