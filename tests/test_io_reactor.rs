#[macro_use(logerr)]
extern crate reactio;

#[cfg(test)]
mod tests {
    use std::io::Write;

    #[test]
    /// SimpleIoReactor implements `Reactor` and calls user handlers on events.
    pub fn test_io_reactor() {
        type AppData = (); // no application data for reactor.
        let app_data = ();
        let addr = "127.0.0.1:12355";
        let recv_buffer_min_size = 1024;
        let mut runtime = reactio::SimpleIoRuntime::new();

        let on_server_sock_msg =
            |buf: &mut [u8], ctx: &mut reactio::SimpleIoReactorContext<'_>, _: &mut AppData| {
                ctx.send_or_que(buf)?; // echo back message.
                Ok(buf.len()) // return number of bytes having been consumed.
            };

        let on_server_connected =
            |ctx: &mut reactio::SimpleIoReactorContext<'_>, listenerid, _: &mut AppData| {
                ctx.cmd_sender
                    .send_close(listenerid, reactio::Deferred::Immediate, |_| {})?; // close parent listerner.
                Ok(()) // accept current connection.
            };

        let on_new_connection = move |_childid| {
            // create a new Reactor for the new connection.
            Some(reactio::SimpleIoReactor::<AppData>::new_boxed(
                app_data,
                Some(Box::new(on_server_connected)), // on_connected
                None,                                // on_closed
                on_server_sock_msg,                  // on_sock_msg
            ))
        };

        //-- server
        runtime
            .get_cmd_sender()
            .send_listen(
                addr,
                reactio::SimpleIoListener::new(recv_buffer_min_size, on_new_connection),
                reactio::Deferred::Immediate,
                |_| {}, // OnCommandCompletion
            )
            .unwrap();
        // wait for server ready.
        let timer = reactio::utils::Timer::new_millis(1000);
        while runtime.count_reactors() < 1 {
            if timer.expired() {
                logerr!("ERROR: timeout waiting for listener start!");
                break;
            }
            runtime.process_events();
        }
        //-- client
        let on_client_connected =
            move |ctx: &mut reactio::SimpleIoReactorContext<'_>, _, _: &mut AppData| {
                // client sends initial msg.
                let mut auto_sender = ctx.acquire_send(); // send on drop
                auto_sender.write_fmt(format_args!("test ")).unwrap();
                auto_sender.write_fmt(format_args!("msgsend")).unwrap();
                assert_eq!(auto_sender.count_written(), 12);
                assert_eq!(auto_sender.get_written(), b"test msgsend");
                // auto_sender.send(None).unwrap(); // this line can be omitted to let it auto send on drop.
                // ctx.send_or_que("Hello".as_bytes())?; // rather than using auto_sender, we call ctx.send_or_que
                Ok(()) // accept connection
            };
        let on_client_sock_msg =
            |_buf: &mut [u8], _ctx: &mut reactio::SimpleIoReactorContext<'_>, _: &mut AppData| {
                Err("Client disconnect on recv response.".to_owned())
            };

        runtime
            .get_cmd_sender()
            .send_connect(
                addr,
                recv_buffer_min_size,
                reactio::SimpleIoReactor::new(
                    app_data,
                    Some(Box::new(on_client_connected)), // on_connected
                    None,                                // on_closed
                    on_client_sock_msg,                  // on_sock_msg
                ),
                reactio::Deferred::Immediate,
                |_| {}, // OnCommandCompletion
            )
            .unwrap();
        // In non-threaded environment, process_events until there're no reactors, no events, no deferred events.
        let timer = reactio::utils::Timer::new_millis(1000);
        while runtime.process_events() {
            if timer.expired() {
                logerr!("ERROR: timeout waiting for tests to complete!");
                break;
            }
        }
        assert_eq!(runtime.count_reactors(), 0);
        assert_eq!(runtime.count_deferred_queue(), 0);
    }

    #[test]
    /// Clonable SimpleIoService implements `Reactor` and serves multiple sockets..
    pub fn test_io_service() {
        struct ServiceData {
            count_recv: usize,
            connt_new_connections: usize,
            count_disconnections: usize,
        }
        type NoAppData = (); // no application data for client.
        let no_app_data = ();
        const NCLIENTS: usize = 3;
        let addr = "127.0.0.1:12355";
        let recv_buf_min_size = 1024;
        let mut runtime = reactio::SimpleIoRuntime::new();

        let on_server_sock_msg = |buf: &mut [u8],
                                  ctx: &mut reactio::SimpleIoReactorContext<'_>,
                                  data: &mut ServiceData| {
            ctx.send_or_que(buf)?; // echo back message.
            data.count_recv += 1;
            Ok(buf.len()) // return number of bytes having been consumed.
        };

        let service = reactio::SimpleIoService::new(
            recv_buf_min_size,
            ServiceData {
                count_recv: 0,
                connt_new_connections: 0,
                count_disconnections: 0,
            },
            // on_connected
            Some(Box::new(
                |_: &mut reactio::SimpleIoReactorContext<'_>,
                 _: reactio::ReactorID,
                 data: &mut ServiceData| {
                    data.connt_new_connections += 1;
                    Ok(())
                },
            )),
            // on_closed
            Some(Box::new(
                |_: reactio::ReactorID, _: &reactio::CmdSender<()>, data: &mut ServiceData| {
                    data.count_disconnections += 1;
                },
            )),
            on_server_sock_msg,
        );
        //-- server
        runtime
            .get_cmd_sender()
            .send_listen(
                addr,
                reactio::SimpleIoListener::new_with_io_service(service.clone()), // clone service on each new connection
                reactio::Deferred::Immediate,
                |_| {}, // OnCommandCompletion
            )
            .unwrap();
        // wait for server ready.
        let timer = reactio::utils::Timer::new_millis(1000);
        while runtime.count_reactors() < 1 {
            if timer.expired() {
                logerr!("ERROR: timeout waiting for listener start!");
                break;
            }
            runtime.process_events();
        }
        //-- client
        let on_client_connected =
            |ctx: &mut reactio::SimpleIoReactorContext<'_>, _, _: &mut NoAppData| {
                // client sends initial msg.
                let mut auto_sender = ctx.acquire_send(); // send on drop
                auto_sender.write_fmt(format_args!("test ")).unwrap();
                auto_sender.write_fmt(format_args!("msgsend")).unwrap();
                assert_eq!(auto_sender.count_written(), 12);
                assert_eq!(auto_sender.get_written(), b"test msgsend");
                // auto_sender.send(None).unwrap(); // this line can be omitted to let it auto send on drop.
                //--- rather than using auto_sender, we call ctx.send_or_que
                // ctx.send_or_que("Hello".as_bytes())?;
                Ok(()) // accept connection
            };
        let on_client_sock_msg =
            |_buf: &mut [u8], _ctx: &mut reactio::SimpleIoReactorContext<'_>, _: &mut NoAppData| {
                Err("Client disconnects when recv response.".to_owned())
            };

        // 3 connections.
        for _ in 0..NCLIENTS {
            runtime
                .get_cmd_sender()
                .send_connect(
                    addr,
                    recv_buf_min_size,
                    reactio::SimpleIoReactor::<NoAppData>::new(
                        no_app_data,
                        Some(Box::new(on_client_connected)), // on_connected
                        None,                                // on_closed
                        on_client_sock_msg,                  // on_sock_msg
                    ),
                    reactio::Deferred::Immediate,
                    |_| {}, // OnCommandCompletion
                )
                .unwrap();
        }

        let timer = reactio::utils::Timer::new_millis(1000);
        let mut disconnections = 0;
        while disconnections < NCLIENTS {
            service
                .apply_app_data(|data| {
                    disconnections = data.count_disconnections;
                })
                .expect("Failed to get app dataa");
            if timer.expired() {
                logerr!("ERROR: timeout waiting for all disconnections!");
                break;
            }
            runtime.process_events();
        }
    }
}
