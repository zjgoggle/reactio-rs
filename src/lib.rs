//! # Portable Reactor pattern in Rust.
//!
//! Supported platforms: Linux, Windows
//!
//! ReactIO is a Rust library that implements event-driven Reactor pattern in single-threaded and multiple-threaded environment.
//! Users must implement their own trait Reactors and add reactors to a ReactRuntime.
//! Each `ReactRuntime` instance runs in a dedicated threaded. It polls all events for managed Reactors. There'are 2 kinds of events:
//! - socket events. We only registers socket READ events, and MsgReader & MsgSender are provided for Reactor to send/receive messages.
//! - commands. Through mpsc channel, reactors could send user defined commands to each other.
//!
//!
//! When processing events, Reactor doesn't need any mutex to protect resources.
//!
//! ## Examples
//!
//! See example in reactor.rs.
//! ```rust,no_run
//!     use reactio::{ReactRuntime, Deferred, DefaultTcpListenerHandler};
//!     use reactio::example;
//!     pub fn test_reactors_cmd() {
//!         let addr = "127.0.0.1:12355";
//!         let mut runtime = ReactRuntime::new();
//!         let cmd_sender = runtime.get_cmd_sender();
//!         cmd_sender
//!             .send_listen(
//!                 addr,
//!                 DefaultTcpListenerHandler::<example::MyReactor>::new(example::ServerParam {
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

#![allow(dead_code)]

pub mod flat_storage;
mod reactor;
pub use reactor::*;
pub mod threaded_reactors;
pub mod utils;
