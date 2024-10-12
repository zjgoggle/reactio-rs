use reactio::{example, logmsg, utils, DefaultTcpListenerHandler, ReactRuntime};

fn run(port: i32) {
    let addr = "127.0.0.1:".to_owned() + &port.to_string();
    let mut runtime = ReactRuntime::new();
    runtime.send_listen(
        &addr,
        DefaultTcpListenerHandler::<example::MyReactor>::new(),
        reactio::Deferred::Immediate,
        |result| match result {
            reactio::CommandCompletion::Error(err) => {
                logmsg!("Failed to listen. err: {}", &err);
            }
            _ => {}
        },
    );
    while runtime.process_events() {}
}
fn main() {
    let port = 10254;
    run(port);
    println!("End of echo_server!");
}
