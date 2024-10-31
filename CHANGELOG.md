# Version 0.1.7
- print pending_read_bytes, pending_send_bytes on close.
- set_log_level, logerr/loginfo/logtrace,

# Version 0.1.6
- Bugfix: try_dispatch_all; AutoSendBuffer::send.

# Version 0.1.5
- Add SimpleIoReactor::new_boxed.
- AutoSendBuffer impl std::io::Write instead of std::fmt::Write

# Version 0.1.4
- Two stage AutoSendBuffer: write messages to send_buf and auto send on drop.

# Version 0.1.3
- Support Result<_, String>. return Err to close reactor.
- Add SimpleIoReactor, SimpleIoListener, CommandReactor.

# Version 0.1.2
- configurable recv_buffer_min_size for MsgReader.
- MsgReader: try_read_fast_read (default) or try_read_fast_dispatch.
- Reactor::on_close: add argument reactorid.
- Reactor::on_command: default panic implementaion.

# Version 0.1.1
- on_inbound_message(new_bytes); fixed test_threaded_reactors;

# Version 0.1.0
- Initial version.
- Event-driven nonblocking reactors.
- Read socket messages.
- Process immediate/deferred user-defined commands.
