//! Ctrl-C in note mode, in a process of its own: once a job asks for notes,
//! SIGINT neither exits nor severs, and is reported exactly once.

use std::io::Read;
use std::os::fd::AsRawFd;

#[test]
fn a_noted_ctrl_c_changes_nothing_but_the_note() {
    let (mut ours, _theirs) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    kevy_sys::install_interrupt(1);
    kevy_sys::sever_on_interrupt(Some(ours.as_raw_fd()));
    kevy_sys::note_interrupts();
    let pid = std::process::id().to_string();
    let sent = std::process::Command::new("kill").args(["-INT", &pid]).status();
    assert!(sent.is_ok_and(|s| s.success()), "kill -INT self");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !kevy_sys::take_noted() {
        assert!(std::time::Instant::now() < deadline, "the handler never ran");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(!kevy_sys::take_noted(), "noted once");
    assert!(!kevy_sys::take_severed(), "noting overrides severing");
    // The connection named for severing is still open.
    ours.set_nonblocking(true).expect("nonblocking");
    let mut buf = [0u8; 1];
    let still_open = ours.read(&mut buf).is_err_and(|e| e.kind() == std::io::ErrorKind::WouldBlock);
    assert!(still_open, "a noted Ctrl-C does not shut the socket down");
}
