//! # Portable Reactor pattern in Rust.
//!
//! Supported platforms: Linux, Windows
//!
//! ReactIO is a Rust library that implements event-driven Reactor pattern in single-threaded and multiple-threaded environment.
//! Each `ReactRuntime` instance runs in a dedicated threaded. It polls all events for managed Reactors. There'are 2 kinds of events:
//! - socket events. We only registers socket READ events, and MsgReader & MsgSender are provided for Reactor to send/receive messages.
//! - commands. Through mpsc channel, reactors could send user defined commands to each other.
//!
//!
//! When processing events, Reactor doesn't need any mutex to protect resources.
//!
//! ## Examples
//!
//! ### Single-threaded ReactRuntime
//!
//! See example in reactor.rs.
//! ```rust,no_run
//!     pub fn test_reactors_cmd() {
//!         let addr = "127.0.0.1:12355";
//!         let mut runtime = ReactRuntime::new();
//!         let cmd_sender = runtime.get_cmd_sender();
//!         cmd_sender
//!             .send_listen(
//!                 addr,
//!                 DefaultTcpListenerHandler::<example::MyReactor>::new(ServerParam {
//!                     name: "server".to_owned(),
//!                     latency_batch: 1000,
//!                 }),
//!                 Deferred::Immediate,
//!                 |_| {},
//!             )
//!             .unwrap();
//!         cmd_sender
//!             .send_connect(
//!                 addr,
//!                 example::MyReactor::new_client("client".to_owned(), 2, 1000),
//!                 Deferred::Immediate,
//!                 |_| {},
//!             )
//!             .unwrap();
//!         // In single threaded environment, process_events until there're no reactors, no events, no deferred events.
//!         while runtime.process_events() {}   
//!         assert_eq!(runtime.count_reactors(), 0);
//!     }
//! ```
//!
//! ### Multi-threaded Reactors - Each thread runs an ReactRuntime
//!
//! See example in threaded_reactors.rs.
//! ```rust,no_run
//!     pub fn test_threaded_reactors() {
//!         let addr = "127.0.0.1:12355";
//!         let stopcounter = Arc::new(AtomicI32::new(0)); // each Reactor increases it when exiting.
//!         let mgr = ThreadedReactorMgr::<String>::new(2); // 2 threads
//!         let (threadid0, threadid1) = (0, 1);
//!         mgr.get_sender(threadid0)
//!             .unwrap()
//!             .send_listen(
//!                 addr,
//!                 create_tcp_listener(ThreadedServerParam {
//!                     runtimeid: threadid0,
//!                     reactormgr: Arc::clone(&mgr),
//!                     stopcounter: Arc::clone(&stopcounter),
//!                     name: "server".to_owned(),
//!                     latency_batch: 1000,
//!                 }),
//!                 Deferred::Immediate,
//!                 |_| {},
//!             )
//!             .unwrap();
//!         mgr.get_sender(threadid1)
//!             .unwrap()
//!             .send_connect(
//!                 addr,
//!                 example::MyThreadedReactor::new_client(
//!                     "myclient".to_owned(),
//!                     threadid1,
//!                     Arc::clone(&mgr),
//!                     5,
//!                     1000,
//!                     Arc::clone(&stopcounter),
//!                 ),
//!                 Deferred::Immediate,
//!                 |_| {},
//!             )
//!             .unwrap();
//!
//!         // wait for 2 reactors exit
//!         while stopcounter.load(atomic::Ordering::Relaxed) != 2 {
//!             std::thread::sleep(std::time::Duration::from_millis(1));
//!             std::thread::yield_now();
//!         }
//!     }
//! ```
//!
//!

#![allow(dead_code)]

pub mod flat_storage;
mod reactor;
pub use reactor::*;
pub mod threaded_reactors;
pub mod utils;
