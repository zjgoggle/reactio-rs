//! # Portable Reactor pattern in Rust.
//!
//! Supported platforms: Linux, Windows
//!
//! ReactIO is a Rust library that implements event-driven Reactor pattern in single-threaded and multiple-threaded environment.
//! Users implement a `Reactor` (as least implement `on_inbound_message`) and add it to a `ReactRuntime`.
//! Each `ReactRuntime` instance runs in a dedicated thread. It polls all events for all managed Reactors. There'are 2 kinds of events:
//! - Socket events. We only register socket READ events initially. MsgReader & MsgSender are provided for Reactor to send/receive messages.
//!   MsgReader reads from buffer and dispatches messages. MsgSender sends or register SEND event for resend when seeing WOULDBLOCK.
//! - Commands. Through mpsc channel, reactors could send user defined commands to each other.
//!
//!
//! Key technologies:
//! - IO events (epoll, iocp): handles IO only when there are event.
//! - MsgSender helps socket read (messages are dispatched when full messages are received); MsgSender handles socket write (it queues unsent messages and auto resends).
//! - No mutex or lock. Just send commands to a mpsc channel owned by `ReactRuntime`.
//! - Deferred commands are executed in a defered time.
//!
//!
//! ## Example
//!
//! ### Non-threaded polling model.
//!
//! See example in reactor.rs.
//! ```rust,no_run
//! use reactio;
//! use std::io::Write;
//!
//! /// SimpleIoReactor implements `Reactor` and calls user-given handlers on events.
//! pub fn test_io_reactor() {
//!     let addr = "127.0.0.1:12355";
//!     let recv_buffer_min_size = 1024;
//!     let mut runtime = reactio::SimpleIoRuntime::new();
//!     let max_echos = 1;
//!     let mut count_echos = 0;
//!
//!     let on_sock_msg = move |buf: &mut [u8], ctx: &mut reactio::SimpleIoReactorContext<'_>| {
//!         if count_echos >= max_echos {
//!             return Err(format!("Reached max echo: {max_echos}")); // close socket
//!         }
//!         ctx.send_msg(buf)?; // echo back message.
//!         count_echos += 1;
//!         Ok(buf.len()) // return number of bytes having been consumed.
//!     };
//!
//!     let on_server_connected = |ctx: &mut reactio::SimpleIoReactorContext<'_>, listenerid| {
//!         ctx.cmd_sender
//!             .send_close(listenerid, reactio::Deferred::Immediate, |_| {})?; // close parent listerner.
//!         Ok(()) // accept current connection.
//!     };
//!
//!     let on_new_connection = move |_childid| {
//!         // create a new Reactor for the new connection.
//!         Some(reactio::SimpleIoReactor::new_boxed(
//!             Some(Box::new(on_server_connected)), // on_connected
//!             None,                                // on_closed
//!             on_sock_msg,                         // on_sock_msg
//!         ))
//!     };
//!
//!     let on_client_connected = |ctx: &mut reactio::SimpleIoReactorContext<'_>, _| {
//!         // client sends initial msg.
//!         let mut auto_sender = ctx.acquire_send(); // send on drop
//!         auto_sender.write_fmt(format_args!("test ")).unwrap();
//!         auto_sender.write_fmt(format_args!("msgsend")).unwrap();
//!         assert_eq!(auto_sender.count_written(), 12);
//!         // auto_sender.send(None).unwrap(); // this line can be omitted to let it auto send on drop.
//!         // ctx.send_msg("Hello".as_bytes())?; // rather than using auto_sender, we call ctx to send_msg
//!         Ok(()) // accept connection
//!     };
//!
//!     //-- server
//!     runtime
//!         .get_cmd_sender()
//!         .send_listen(
//!             addr,
//!             reactio::SimpleIoListener::new(recv_buffer_min_size, on_new_connection),
//!             reactio::Deferred::Immediate,
//!             |_| {},  //OnCommandCompletion
//!         )
//!         .unwrap();
//!     // wait for server ready.
//!     let timer = reactio::utils::Timer::new_millis(1000);
//!     while runtime.count_reactors() < 1 {
//!         if timer.expired() {
//!             assert!(false, "ERROR: timeout waiting for listener start!");
//!             break;
//!         }
//!         runtime.process_events();
//!     }
//!     //-- client
//!     runtime
//!         .get_cmd_sender()
//!         .send_connect(
//!             addr,
//!             recv_buffer_min_size,
//!             reactio::SimpleIoReactor::new(
//!                 Some(Box::new(on_client_connected)), // on_connected
//!                 None,                                // on_closed
//!                 on_sock_msg,                         // on_sock_msg
//!             ),
//!             reactio::Deferred::Immediate,
//!             |_| {},  // OnCommandCompletion
//!         )
//!         .unwrap();
//!     // In non-threaded environment, process_events until there're no reactors, no events, no deferred events.
//!     let timer = reactio::utils::Timer::new_millis(1000);
//!     while runtime.process_events() {
//!         if timer.expired() {
//!             assert!(false, "ERROR: timeout waiting for tests to complete!");
//!             break;
//!         }
//!     }
//!     assert_eq!(runtime.count_reactors(), 0);
//!     assert_eq!(runtime.count_deferred_queue(), 0);
//! }
//! ```
//!
//! More examples are echo_client/echo_server (for TCP latency test), PingpongReactor, ThreadedPingpongReactor.
//!

#![allow(dead_code)]

pub mod flat_storage;
mod reactor;
pub use reactor::*;
pub mod threaded_reactors;
pub mod utils;
