use reactio::poller::{sample, DefaultTcpListenerHandler, SocketPoller};

fn run(port: i32) {
    let addr = "127.0.0.1:".to_owned() + &port.to_string();
    let mut mgr = SocketPoller::new();
    mgr.start_listen(
        &addr,
        DefaultTcpListenerHandler::<sample::TcpEchoHandler>::new_boxed(),
    )
    .unwrap();
    println!("Started server. Ctl+C to exit.");
    while mgr.count_streams() == 0 {
        mgr.process_events();
    }
    loop {
        mgr.process_events(); // for ever
    }
    // remaining active listener
}
fn main() {
    let port = 10254;
    run(port);
    println!("Hello from an echo_server!");
}
