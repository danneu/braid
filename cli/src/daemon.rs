use crate::cmd::RealRunner;
use crate::config::config_read;
use crate::probe::RealFilesystem;
use crate::protocol::{Request, Response};
use crate::status::build_status_report;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub fn run_daemon() {
    let listener = listener_from_systemd();

    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown))
        .expect("failed to register SIGTERM handler");

    listener
        .set_nonblocking(true)
        .expect("failed to set listener to non-blocking");

    eprintln!("braid daemon: listening via systemd socket activation");

    loop {
        if shutdown.load(Ordering::Relaxed) {
            eprintln!("braid daemon: shutting down");
            break;
        }

        match listener.accept() {
            Ok((stream, _addr)) => {
                std::thread::spawn(move || {
                    handle_connection(stream);
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("braid daemon: accept error: {e}");
            }
        }
    }
}

fn listener_from_systemd() -> UnixListener {
    let listen_pid: u32 = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            eprintln!("error: LISTEN_PID not set — braid daemon requires systemd socket activation");
            std::process::exit(1);
        });

    let listen_fds: u32 = std::env::var("LISTEN_FDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            eprintln!("error: LISTEN_FDS not set — braid daemon requires systemd socket activation");
            std::process::exit(1);
        });

    if listen_pid != std::process::id() {
        eprintln!(
            "error: LISTEN_PID ({listen_pid}) does not match current PID ({})",
            std::process::id()
        );
        std::process::exit(1);
    }

    if listen_fds != 1 {
        eprintln!("error: expected exactly 1 socket fd, got {listen_fds}");
        std::process::exit(1);
    }

    // fd 3 is the first socket activation fd per sd_listen_fds(3)
    //
    // Validate fd 3 is a Unix socket before taking ownership.
    // fstat tells us the fd type without side effects.
    let fd = 3i32;
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        eprintln!("error: fstat(3) failed — fd 3 is not open");
        std::process::exit(1);
    }
    if (stat.st_mode & libc::S_IFMT) != libc::S_IFSOCK {
        eprintln!("error: fd 3 is not a socket (mode: {:#o})", stat.st_mode);
        std::process::exit(1);
    }

    let listener = unsafe { UnixListener::from_raw_fd(fd) };

    // Verify it's a Unix domain socket (AF_UNIX) and is listening
    listener
        .local_addr()
        .expect("error: fd 3 is not a Unix domain socket — check systemd socket config");

    // Verify it's actually in listening state by checking getsockopt(SO_ACCEPTCONN)
    let accept_conn: libc::c_int = unsafe {
        let mut val: libc::c_int = 0;
        let mut len: libc::socklen_t = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let ret = libc::getsockopt(
            listener.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_ACCEPTCONN,
            &mut val as *mut _ as *mut libc::c_void,
            &mut len,
        );
        if ret != 0 {
            eprintln!("error: getsockopt(SO_ACCEPTCONN) failed on fd 3");
            std::process::exit(1);
        }
        val
    };
    if accept_conn == 0 {
        eprintln!("error: fd 3 is a socket but not in listening state");
        std::process::exit(1);
    }

    listener
}

fn handle_status() -> Response {
    let config = match config_read(std::path::Path::new("/etc/braid/config.json")) {
        Ok(c) => c,
        Err(e) => return Response::err(format!("config: {e}")),
    };
    let runner = RealRunner;
    let fs = RealFilesystem;
    match build_status_report(&runner, &fs, &config) {
        Ok(report) => match serde_json::to_value(&report) {
            Ok(data) => Response::ok_with_data(data),
            Err(e) => Response::err(format!("serialize: {e}")),
        },
        Err(e) => Response::err(format!("status: {e}")),
    }
}

fn handle_connection(stream: UnixStream) {
    // Cap reads at 64 KiB to prevent unbounded memory usage
    let writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("braid daemon: stream clone error: {e}");
            return;
        }
    };
    let reader = BufReader::new(stream.take(64 * 1024));
    let mut writer = writer;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("braid daemon: read error: {e}");
                break;
            }
        };

        if line.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => match req {
                Request::Ping => Response::ok(),
                Request::Status => handle_status(),
            },
            Err(_) => Response::err("invalid request"),
        };
        let response = serde_json::to_string(&response).unwrap();

        if let Err(e) = writeln!(writer, "{response}") {
            eprintln!("braid daemon: write error: {e}");
            break;
        }
    }
}
