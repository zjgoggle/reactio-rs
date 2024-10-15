use reactio::{example, logmsg, utils, DefaultTcpListenerHandler, ReactRuntime};

fn run(port: i32) {
    let addr = "127.0.0.1:".to_owned() + &port.to_string();
    let mut runtime = ReactRuntime::new();
    let cmd_sender = runtime.get_cmd_sender();
    cmd_sender
        .send_listen(
            &addr,
            DefaultTcpListenerHandler::<example::MyReactor>::new(example::ServiceParam {
                name: "server".to_owned(),
                latency_batch: 1000,
            }),
            reactio::Deferred::Immediate,
            |result| match result {
                reactio::CommandCompletion::Error(err) => {
                    logmsg!("Failed to listen. err: {}", &err);
                }
                _ => {}
            },
        )
        .unwrap();
    while runtime.process_events() {}
    assert_eq!(runtime.count_reactors(), 0);
}
fn main() {
    let port = 10254;
    run(port);
    println!("End of echo_server!");
}
