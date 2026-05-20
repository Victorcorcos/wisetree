//! Wrapper-mode behavior tests. The full end-to-end scenario (PTY harness
//! → captured stdout) requires `script(1)` / a Unix-only PTY library; here
//! we cover the contract that doesn't need a real TTY:
//!
//! - `App::new(_, true)` exposes `is_from_wrapper = true`.
//! - A freshly-constructed app reports no `selected_path`.
//! - The `terminal::enter_wrapper` symbol is reachable (compile-time check).

use wisetree::cli::AppMode;
use wisetree::tui::App;

#[test]
fn wrapper_flag_propagates_to_app() {
    let app = App::new(AppMode::Dashboard, true);
    assert!(app.is_from_wrapper);
    assert!(app.selected_path().is_none());
}

#[test]
fn non_wrapper_app_is_marked_correctly() {
    let app = App::new(AppMode::Menu, false);
    assert!(!app.is_from_wrapper);
    assert!(app.selected_path().is_none());
}

#[test]
fn wrapper_terminal_constructor_is_callable() {
    // Compile-time check: the symbol exists and has the expected
    // signature. We don't actually open `/dev/tty` here because cargo
    // test is not guaranteed a controlling terminal.
    let _: fn() -> std::io::Result<wisetree::tui::terminal::WrapperTerminal> =
        wisetree::tui::terminal::enter_wrapper;
}

#[cfg(unix)]
mod unix_shutdown {
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use assert_cmd::cargo::cargo_bin;

    struct WrapperProcess {
        child: Child,
        master: File,
    }

    fn duplicate_stdio(file: &File) -> io::Result<Stdio> {
        let fd = unsafe { libc::dup(file.as_raw_fd()) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Stdio::from(unsafe { File::from_raw_fd(fd) }))
    }

    fn set_cloexec(fd: libc::c_int) -> io::Result<()> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn open_pty() -> io::Result<(File, File)> {
        let mut master = -1;
        let mut slave = -1;
        let mut winsize = libc::winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let result = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut winsize as *mut _,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        set_cloexec(master)?;
        set_cloexec(slave)?;
        Ok((unsafe { File::from_raw_fd(master) }, unsafe {
            File::from_raw_fd(slave)
        }))
    }

    fn spawn_wrapper_process() -> io::Result<WrapperProcess> {
        let binary = cargo_bin("wisetree");
        let (master, slave) = open_pty()?;
        let slave_fd = slave.as_raw_fd();
        let mut cmd = Command::new(binary);
        cmd.arg("--from-wrapper")
            .stdin(duplicate_stdio(&slave)?)
            .stdout(Stdio::piped())
            .stderr(duplicate_stdio(&slave)?);
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(slave_fd, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        drop(slave);
        Ok(WrapperProcess { child, master })
    }

    fn read_available(master: &File, buffer: &mut Vec<u8>) -> io::Result<usize> {
        let mut total = 0usize;
        loop {
            let mut chunk = [0_u8; 4096];
            let read = unsafe {
                libc::read(
                    master.as_raw_fd(),
                    chunk.as_mut_ptr().cast::<libc::c_void>(),
                    chunk.len(),
                )
            };
            if read == 0 {
                return Ok(total);
            }
            if read > 0 {
                let n = read as usize;
                buffer.extend_from_slice(&chunk[..n]);
                total += n;
                continue;
            }
            let err = io::Error::last_os_error();
            match err.kind() {
                io::ErrorKind::WouldBlock => return Ok(total),
                _ => return Err(err),
            }
        }
    }

    fn wait_for_exit(child: &mut Child, timeout: Duration) -> io::Result<std::process::ExitStatus> {
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            if started.elapsed() >= timeout {
                terminate_child(child);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "wrapper process did not exit after PTY close",
                ));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn set_nonblocking(file: &File) -> io::Result<()> {
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn terminate_child(child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    fn wait_for_tty_bytes(
        session: &mut WrapperProcess,
        needle: &[u8],
        timeout: Duration,
    ) -> io::Result<Vec<u8>> {
        set_nonblocking(&session.master)?;
        let started = Instant::now();
        let mut tty_bytes = Vec::new();
        while started.elapsed() < timeout {
            let _ = read_available(&session.master, &mut tty_bytes)?;
            if contains_bytes(&tty_bytes, needle) {
                return Ok(tty_bytes);
            }
            if session.child.try_wait()?.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("timed out waiting for tty bytes {:?}", needle),
        ))
    }

    #[test]
    fn wrapper_mode_exits_promptly_when_the_terminal_disappears() -> io::Result<()> {
        for delay in [
            Duration::ZERO,
            Duration::from_millis(10),
            Duration::from_millis(50),
        ] {
            for _ in 0..3 {
                let mut session = spawn_wrapper_process()?;
                thread::sleep(delay);
                drop(session.master);
                let _ = wait_for_exit(&mut session.child, Duration::from_secs(3))?;
            }
        }
        Ok(())
    }

    #[test]
    fn direct_wrapper_mode_stays_open_after_rendering_loading_screen() -> io::Result<()> {
        let mut session = spawn_wrapper_process()?;
        let _ = wait_for_tty_bytes(&mut session, b"Loading", Duration::from_secs(2))?;

        // Stay open long enough to cross the orphan-watchdog interval. The
        // regression this covers rendered the first frame, then self-closed
        // roughly 0.5-1.5s later when the watchdog falsely judged the tty dead.
        thread::sleep(Duration::from_millis(900));
        assert!(
            session.child.try_wait()?.is_none(),
            "wrapper mode exited on its own after rendering the loading screen"
        );

        drop(session.master);
        let _ = wait_for_exit(&mut session.child, Duration::from_secs(3))?;
        Ok(())
    }

    #[test]
    fn wrapper_mode_exits_promptly_after_entering_dashboard_then_losing_terminal() -> io::Result<()>
    {
        let mut session = spawn_wrapper_process()?;
        let menu_bytes = wait_for_tty_bytes(&mut session, b"Dashboard", Duration::from_secs(2))?;
        assert!(contains_bytes(&menu_bytes, b"Create"));

        // Menu order is Create -> Dashboard, so one Down + Enter opens the live dashboard.
        let _ = unsafe {
            libc::write(
                session.master.as_raw_fd(),
                b"\x1b[B\r".as_ptr().cast::<libc::c_void>(),
                4,
            )
        };
        thread::sleep(Duration::from_millis(300));

        let started = Instant::now();
        drop(session.master);
        let _ = wait_for_exit(&mut session.child, Duration::from_secs(3))?;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "wrapper process lingered too long after dashboard terminal close"
        );
        Ok(())
    }

    /// Real-world scenario: the wisetree shell wrapper invokes us via
    /// command substitution (`dir=$(... wisetree --from-wrapper)`), so the
    /// immediate parent is a bash subshell, not the test harness. When the
    /// user closes the terminal tab, Terminal.app closes the master pty —
    /// at which point the bash subshell, the wrapper shell, AND wisetree
    /// itself all lose the slave's underlying device.
    ///
    /// Reproducing that path here ensures the prompt-exit behavior holds
    /// even when wisetree is not the session leader and even when no
    /// SIGHUP propagates down the process chain.
    /// Returns the pid of any `wisetree --from-wrapper` process matching
    /// `binary`. We restrict to *this* test binary to avoid catching unrelated
    /// wisetree invocations the dev might have running.
    fn pgrep_wisetree(binary: &std::path::Path) -> Vec<i32> {
        let needle = binary.to_string_lossy().to_string();
        let output = Command::new("pgrep")
            .arg("-f")
            .arg(format!("{}.*--from-wrapper", needle))
            .output();
        let Ok(output) = output else {
            return Vec::new();
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().parse::<i32>().ok())
            .collect()
    }

    /// Wait for every wisetree process spawned for the given binary to exit.
    /// Returns the leftover pids that didn't die within `timeout`.
    fn wait_for_wisetree_orphans_to_die(binary: &std::path::Path, timeout: Duration) -> Vec<i32> {
        let started = Instant::now();
        loop {
            let pids = pgrep_wisetree(binary);
            if pids.is_empty() || started.elapsed() >= timeout {
                return pids;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn spawn_wrapper_via_bash() -> io::Result<WrapperProcess> {
        let binary = cargo_bin("wisetree");
        let script = format!(
            "dir=$(FORCE_COLOR=3 {} --from-wrapper); echo CAPTURED=$dir",
            binary.display()
        );

        let (master, slave) = open_pty()?;
        let slave_fd = slave.as_raw_fd();
        let mut cmd = Command::new("/bin/bash");
        cmd.arg("-c")
            .arg(script)
            .stdin(duplicate_stdio(&slave)?)
            .stdout(duplicate_stdio(&slave)?)
            .stderr(duplicate_stdio(&slave)?);
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(slave_fd, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        drop(slave);
        Ok(WrapperProcess { child, master })
    }

    #[test]
    fn wrapper_under_bash_subshell_exits_promptly_after_terminal_close() -> io::Result<()> {
        let binary = cargo_bin("wisetree");
        let mut session = spawn_wrapper_via_bash()?;
        // The wisetree menu renders into the bash session's tty. Wait for
        // it to appear so we know the binary actually started.
        let menu_bytes = wait_for_tty_bytes(&mut session, b"Dashboard", Duration::from_secs(5))?;
        assert!(contains_bytes(&menu_bytes, b"Create"));

        // Enter the dashboard (Down + Enter) so the live render path is
        // active when we yank the terminal — that's the path the original
        // bug report hit.
        let _ = unsafe {
            libc::write(
                session.master.as_raw_fd(),
                b"\x1b[B\r".as_ptr().cast::<libc::c_void>(),
                4,
            )
        };
        thread::sleep(Duration::from_millis(500));

        let started = Instant::now();
        drop(session.master);
        // Allow generous slack (orphan watchdog forces _exit after ~1s) but
        // still well under the 60s pty timeout that produced the original
        // "terminal frozen for a minute" symptom.
        let _ = wait_for_exit(&mut session.child, Duration::from_secs(5))?;
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "bash-wrapped wisetree lingered too long after terminal close: {:?}",
            started.elapsed()
        );

        // Bash dying isn't enough — the *wisetree* process must also exit.
        // The user-visible bug is wisetree orphans alive at 1% CPU after
        // the terminal closes, even when bash itself is gone. pgrep gives
        // us the ground truth about leftover wisetree processes.
        let leftover = wait_for_wisetree_orphans_to_die(&binary, Duration::from_secs(3));
        assert!(
            leftover.is_empty(),
            "wisetree orphan(s) survived bash exit: {leftover:?}"
        );
        Ok(())
    }

    /// Two back-to-back bash-wrapped invocations, mirroring the user's
    /// reproduction (Cmd+W after the first run, then `wisetree` again, then
    /// Cmd+W again). Each iteration must leave zero orphans behind.
    #[test]
    fn back_to_back_bash_wrapped_invocations_leave_no_orphans() -> io::Result<()> {
        let binary = cargo_bin("wisetree");
        for iteration in 0..2 {
            let mut session = spawn_wrapper_via_bash()?;
            let _ = wait_for_tty_bytes(&mut session, b"Dashboard", Duration::from_secs(5))?;
            let _ = unsafe {
                libc::write(
                    session.master.as_raw_fd(),
                    b"\x1b[B\r".as_ptr().cast::<libc::c_void>(),
                    4,
                )
            };
            thread::sleep(Duration::from_millis(300));
            drop(session.master);
            let _ = wait_for_exit(&mut session.child, Duration::from_secs(5))?;
            let leftover = wait_for_wisetree_orphans_to_die(&binary, Duration::from_secs(3));
            assert!(
                leftover.is_empty(),
                "iteration {iteration}: wisetree orphan(s) survived: {leftover:?}"
            );
        }
        Ok(())
    }
}
