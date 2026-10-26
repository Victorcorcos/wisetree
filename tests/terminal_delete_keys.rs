//! Exercise Crossterm's real Unix parser through a PTY, rather than constructing
//! the key events that the dashboard expects by hand.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers};
use wisetree::tui::event::{Event, EventLoop};

const CHILD_ENV: &str = "WISETREE_TEST_DELETE_KEYS_PTY";

#[test]
fn terminal_delete_key_reader() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    crossterm::terminal::enable_raw_mode().unwrap();
    println!("KEY_READER_READY");
    std::io::stdout().flush().unwrap();
    let mut events = EventLoop::new(Duration::from_millis(50));
    let expected = [
        (KeyCode::Char('x'), KeyModifiers::CONTROL),
        (KeyCode::Backspace, KeyModifiers::NONE),
        (KeyCode::Delete, KeyModifiers::NONE),
        (KeyCode::Char('h'), KeyModifiers::CONTROL),
        (KeyCode::Backspace, KeyModifiers::CONTROL),
        (KeyCode::Delete, KeyModifiers::CONTROL),
    ];
    for (code, modifiers) in expected {
        loop {
            if let Event::Key(key) = events.next_event().unwrap() {
                assert_eq!((key.code, key.modifiers), (code, modifiers));
                break;
            }
        }
    }
    crossterm::terminal::disable_raw_mode().unwrap();
}

#[test]
fn terminal_distinguishes_ctrl_x_from_search_editing_keys() {
    let mut master = -1;
    let mut slave = -1;
    // SAFETY: openpty initializes the two file descriptors. Optional name,
    // termios and window-size pointers are null, requesting system defaults.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
    // SAFETY: successful openpty returned distinct, owned file descriptors.
    let mut master = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(master) });
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "terminal_delete_key_reader", "--nocapture"])
        .env(CHILD_ENV, "1")
        .stdin(Stdio::from(slave))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(output.read_line(&mut line).unwrap(), 0, "reader exited");
        if line.contains("KEY_READER_READY") {
            break;
        }
    }
    // Default Ctrl+X (0x18) is distinct from all the search editing keys:
    // Backspace, Delete, Ctrl+H, Ctrl+Backspace, and Ctrl+Delete.
    master
        .write_all(b"\x18\x7f\x1b[3~\x08\x1b[127;5u\x1b[3;5~")
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("terminal key reader timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut remaining = String::new();
    output.read_to_string(&mut remaining).unwrap();
    let mut errors = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut errors)
        .unwrap();
    assert!(status.success(), "{remaining}\n{errors}");
}
