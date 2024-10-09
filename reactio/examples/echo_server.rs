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
    while runtime.process_events() {}
}
fn main() {
    let port = 10254;
    run(port);
    println!("End of echo_server!");
}
