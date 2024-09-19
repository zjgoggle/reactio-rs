use reactio::poller::{sample, SocketPoller};

fn run(port: i32, max_echos: i32, latency_batch: i32) {
    let addr = "127.0.0.1:".to_owned() + &port.to_string();
    let mut mgr = SocketPoller::new();
    mgr.start_connect(
        &addr,
        sample::TcpEchoHandler::new_client(max_echos, latency_batch),
    )
    .unwrap();

    while mgr.count_streams() > 0 {
        mgr.process_events();
    }
    assert_eq!(mgr.len(), 0);
}
fn main() {
    let port = 10254;
    let latency_batch = 10000;
    let max_echos = 100000;
    run(port, max_echos, latency_batch);
    println!("Hello from an echo_server!");
}
