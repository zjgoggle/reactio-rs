# ReactIO

[![Build](https://github.com/zjgoggle/reactio-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/zjgoggle/reactio-rs/actions)
[![License](https://img.shields.io/badge/license-Apache--2.0_OR_MIT-blue.svg)](https://github.com/zjgoggle/reactio-rs)
[![Cargo](https://img.shields.io/crates/v/reactio.svg)](https://crates.io/crates/reactio)
[![Documentation](https://docs.rs/reactio/badge.svg)](https://docs.rs/reactio)

Portable Reactor pattern in Rust.

Supported platforms: Linux, Windows     **Only 64-bit platforms are supported**

ReactIO is a Rust library that implements event-driven Reactor pattern in single-threaded and multiple-threaded environment.
Each `ReactRuntime` instance runs in a dedicated thread. It polls all events for managed Reactors. There'are 2 kinds of events: 
- socket events. We only register socket READ events initially. MsgReader & MsgSender are provided for Reactor to send/receive messages.
- commands. Through mpsc channel, reactors could send user defined commands to each other.


When processing events, Reactor doesn't need any mutex to protect resources.

## Examples

### Single-threaded ReactRuntime

See example in reactor.rs.
```rust,no_run
    pub fn test_reactors_cmd() {
        let addr = "127.0.0.1:12355";
        let mut runtime = ReactRuntime::new();
        let cmd_sender = runtime.get_cmd_sender();
        cmd_sender
            .send_listen(
                addr,
                DefaultTcpListenerHandler::<example::MyReactor>::new(ServerParam {
                    name: "server".to_owned(),
                    latency_batch: 1000,
                }),
                Deferred::Immediate,
                |_| {},
            )
            .unwrap();
        cmd_sender
            .send_connect(
                addr,
                example::MyReactor::new_client("client".to_owned(), 2, 1000),
                Deferred::Immediate,
                |_| {},
            )
            .unwrap();
        // In single threaded environment, process_events until there're no reactors, no events, no deferred events.
        while runtime.process_events() {}   
        assert_eq!(runtime.count_reactors(), 0);
    }
```

### Multi-threaded Reactors - Each thread runs an ReactRuntime

See example in threaded_reactors.rs.
```rust,no_run
pub fn test_threaded_reactors() {
        let addr = "127.0.0.1:12355";
        let stopcounter = Arc::new(AtomicI32::new(0)); // each Reactor increases it when exiting.
        let mgr = ThreadedReactorMgr::<String>::new(2); // 2 threads
        let (threadid0, threadid1) = (0, 1);

        // cloned Arc are passed to threads.
        let (amgr, astopcounter) = (Arc::clone(&mgr), Arc::clone(&stopcounter));

        mgr.get_cmd_sender(threadid0)
            .unwrap()
            .send_listen(
                addr,
                create_tcp_listener(ThreadedServerParam {
                    runtimeid: threadid0,
                    reactormgr: Arc::clone(&mgr),
                    stopcounter: Arc::clone(&stopcounter),
                    name: "server".to_owned(),
                    latency_batch: 1000,
                }),
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
```

## License

Licensed under either of

 * Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or https://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or https://opensource.org/license/mit/)

at your option.

#### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
