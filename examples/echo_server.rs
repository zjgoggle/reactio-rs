use reactio::{logmsg, DefaultTcpListenerHandler, ReactRuntime};
mod example;

fn run(port: i32) {
    let addr = "127.0.0.1:".to_owned() + &port.to_string();
    let recv_buffer_min_size = 1024;
    let mut runtime = ReactRuntime::new();
    let cmd_sender = runtime.get_cmd_sender();
    cmd_sender
        .send_listen(
            &addr,
            DefaultTcpListenerHandler::<example::pingpong::PingpongReactor>::new(
                recv_buffer_min_size,
                example::pingpong::ServerParam {
                    name: "server".to_owned(),
                    latency_batch: 1000,
                },
            ),
            reactio::Deferred::Immediate,
            |result| {
                if let reactio::CommandCompletion::Error(err) = result {
                    logmsg!("Failed to listen. err: {}", err);
                }
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
