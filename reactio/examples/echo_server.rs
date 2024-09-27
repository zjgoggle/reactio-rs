use reactio::{sample, DefaultTcpListenerHandler, ReactRuntime};

fn run(port: i32) {
    let addr = "127.0.0.1:".to_owned() + &port.to_string();
    let mut runtime = ReactRuntime::new();
    runtime
        .start_listen(
            &addr,
            DefaultTcpListenerHandler::<sample::MyReactor>::new_boxed(),
        )
        .unwrap();
    println!("Started server. Ctl+C to exit.");
    while runtime.count_streams() == 0 {
        runtime.process_events();
    }
    loop {
        runtime.process_events(); // for ever
    }
    // remaining active listener
}
fn main() {
    let port = 10254;
    run(port);
    println!("Hello from an echo_server!");
}
