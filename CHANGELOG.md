# Version 0.1.2
- configurable recv_buffer_min_size for MsgReader.
- MsgReader: try_read_fast_read (default) or try_read_fast_dispatch
- Reactor::on_close: add argument reactorid
- Reactor::on_command: default panic implementaion.

# Version 0.1.1
- on_inbound_message(new_bytes); fixed test_threaded_reactors;

# Version 0.1.0
- Initial version