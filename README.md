# ReactIO

[![Build](https://github.com/zjgoggle/reactio-rs/actions/workflows/build-test.yml/badge.svg)](https://github.com/zjgoggle/reactio-rs/actions)
[![License](https://img.shields.io/badge/license-Apache--2.0_OR_MIT-blue.svg)](https://github.com/zjgoggle/reactio-rs)
[![Cargo](https://img.shields.io/crates/v/reactio.svg)](https://crates.io/crates/reactio)
[![depstatus](https://deps.rs/repo/github/zjgoggle/reactio-rs/status.svg)](https://deps.rs/repo/github/zjgoggle/reactio-rs)
[![Crates.io](https://img.shields.io/crates/d/reactio)](https://github.com/zjgoggle/reactio-rs)
[![Documentation](https://docs.rs/reactio/badge.svg)](https://docs.rs/reactio)

Low-latency Event-driven Non-blocking Reactor pattern in Rust.

Supported platforms: Linux, Windows and x86_64, arm64. (Other platforms are not tested.)

**Only 64-bit platforms are supported**

ReactIO impements a low-latency event-driven Reactor pattern in non-threaded and multiple-threaded environment.
Users implement a `Reactor` (as least implement `on_inbound_message`) and add it to a `ReactRuntime`.
Each `ReactRuntime` instance runs in a dedicated thread. It polls all events for managed Reactors. There'are 2 kinds of events: 
- Socket events. We only register socket READ events initially. MsgReader & MsgSender are provided for Reactor to send/receive messages.
- Commands. Through mpsc channel, reactors could send user defined commands to each other.

There's another set of structs - SimpleIoReactor, SimpleIoListener, CommandReactor - when people only want to supply event functions. See below examples.

Key technologies:
- IO events (epoll, iocp): handles IO only when there are event.
- MsgSender helps socket read (messages are dispatched when full messages are received); MsgSender handles socket write (it queues unsent messages and auto resends).
- No/minimum mutexes or locks. Just send commands to a mpsc channel owned by a `ReactRuntime`.
- Deferred commands are executed in a defered time.
- No heap allocated objects on receiving events (it diffs from async frameworks which create Futures on heap).

When processing events, Reactor doesn't need any mutex to protect resources.

## Examples

### None-threaded ReactRuntime

#### Example 1: Define a struct PingpongReactor to implement Reactor.

More tests are in [tests](tests) and [examples](examples).

```rust,no_run
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
                    name: "server".to_owned(), // parent/listner reactor name. Children names are appended a count number. E.g. "Server-1" for the first connection.
                    latency_batch: 1000, // report round-trip time for each latency_batch samples.
                },
            ),
            Deferred::Immediate,
            |_| {},  // OnCommandCompletion
        )
        .unwrap();
    cmd_sender
        .send_connect(
            addr,
            recv_buffer_min_size,
            // client PingpongReactor initiate a message. It sends echo back 2 messages before close and latency_batch=1000.
            PingpongReactor::new_client("client".to_owned(), 2, 1000),
            Deferred::Immediate,
            |_| {},  // OnCommandCompletion
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
```

#### Example 2: User supplies callback functions to Reactor instance to handle socket messages.

```rust,norun
/// SimpleIoReactor implements `Reactor` and calls user handlers on events.
pub fn test_io_reactor() {
    let addr = "127.0.0.1:12355";
    let recv_buffer_min_size = 1024;
    let mut runtime = reactio::SimpleIoRuntime::new();

    let on_sock_msg = {
        let max_echos = 1;
        let mut count_echos = 0;
        move |buf: &mut [u8], ctx: &mut reactio::SimpleIoReactorContext<'_>| {
            if count_echos >= max_echos {
                return Err(format!("Reached max echo: {max_echos}")); // close socket
            }
            ctx.send_or_que(buf)?; // echo back message.
            count_echos += 1;
            Ok(buf.len()) // return number of bytes having been consumed.
        }
    };

    let on_server_connected = |ctx: &mut reactio::SimpleIoReactorContext<'_>, listenerid| {
        ctx.cmd_sender
            .send_close(listenerid, reactio::Deferred::Immediate, |_| {})?; // close parent listerner.
        Ok(()) // accept current connection.
    };

    let on_new_connection = move |_childid| {
        // create a new Reactor for the new connection.
        Some(reactio::SimpleIoReactor::new_boxed(
            Some(Box::new(on_server_connected)), // on_connected
            None,                                // on_closed
            on_sock_msg,                         // on_sock_msg
        ))
    };

    let on_client_connected = |ctx: &mut reactio::SimpleIoReactorContext<'_>, _| {
        // client sends initial msg.
        let mut auto_sender = ctx.acquire_send(); // send on drop
        auto_sender.write_fmt(format_args!("test ")).unwrap();
        auto_sender.write_fmt(format_args!("msgsend")).unwrap();
        assert_eq!(auto_sender.count_written(), 12);
        assert_eq!(auto_sender.get_written(), b"test msgsend");
        // auto_sender.send(None).unwrap(); // this line can be omitted to let it auto send on drop.
        // ctx.send_or_que("Hello".as_bytes())?; // rather than using auto_sender, we call ctx.send_or_que
        Ok(()) // accept connection
    };

    //-- server
    runtime
        .get_cmd_sender()
        .send_listen(
            addr,
            reactio::SimpleIoListener::new(recv_buffer_min_size, on_new_connection),
            reactio::Deferred::Immediate,
            |_| {}, // OnCommandCompletion
        )
        .unwrap();
    // wait for server ready.
    let timer = reactio::utils::Timer::new_millis(1000);
    while runtime.count_reactors() < 1 {
        if timer.expired() {
            logerr!("ERROR: timeout waiting for listener start!");
            break;
        }
        runtime.process_events();
    }
    //-- client
    runtime
        .get_cmd_sender()
        .send_connect(
            addr,
            recv_buffer_min_size,
            reactio::SimpleIoReactor::new(
                Some(Box::new(on_client_connected)), // on_connected
                None,                                // on_closed
                on_sock_msg,                         // on_sock_msg
            ),
            reactio::Deferred::Immediate,
            |_| {}, // OnCommandCompletion
        )
        .unwrap();
    // In non-threaded environment, process_events until there're no reactors, no events, no deferred events.
    let timer = reactio::utils::Timer::new_millis(1000);
    while runtime.process_events() {
        if timer.expired() {
            logerr!("ERROR: timeout waiting for tests to complete!");
            break;
        }
    }
    assert_eq!(runtime.count_reactors(), 0);
    assert_eq!(runtime.count_deferred_queue(), 0);
}
```

### Multi-threaded Reactor Example - Each thread runs an ReactRuntime

See example in `threaded_pingpong.rs`.
```rust,no_run
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
            // OnCommandCompletion, when listen socket is ready, send another command to connect from another thread.
            move |res| {
                if let Err(err) = res {
                    logerr!("[ERROR] Failed to listen. {err}");
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
                            if let Err(err) = res {
                                logerr!("Failed connect! {err}");
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
