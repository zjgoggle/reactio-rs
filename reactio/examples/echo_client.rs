use reactio::{sample, ReactRuntime};

fn run(port: i32, max_echos: i32, latency_batch: i32) {
    let addr = "127.0.0.1:".to_owned() + &port.to_string();
    let mut runtime = ReactRuntime::new();
    runtime
        .start_connect(
            &addr,
            sample::MyReactor::new_client(max_echos, latency_batch),
        )
        .unwrap();

    while runtime.count_streams() > 0 {
        runtime.process_events();
    }
    assert_eq!(runtime.len(), 0);
}
fn main() {
    let port = 10254;
    let latency_batch = 10000;
    let max_echos = 100000;
    run(port, max_echos, latency_batch);
    println!("Hello from an echo_server!");
}
