//! `agentmux-server` binary — CLI plumbing only; the daemon itself lives
//! in `agentmux_server::rpc_server`.
//!
//! ```text
//! agentmux-server --serve   [--socket PATH] [--data-dir PATH] [--config PATH]
//! agentmux-server --daemon  [--socket PATH] [--data-dir PATH] [--config PATH]
//! ```
//!
//! `--serve` runs the daemon in the foreground (used directly, and by
//! `--daemon`'s detached child). `--daemon` is the TUI's auto-start path:
//! it re-spawns this executable with `--serve`, stdio detached
//! (`Stdio::null()`), in a new process group (`setpgid`-equivalent — a
//! documented "daemonize-lite": the child survives the parent's exit and
//! terminal teardown without a full `setsid`/double-fork dance).

use std::env;
use std::path::PathBuf;
use std::process::{exit, Command, Stdio};

use agentmux_server::{bind_unix_listener, build_daemon, lock_data_dir, ServerPaths};

enum Mode {
    Serve,
    Daemon,
}

fn usage() -> ! {
    eprintln!(
        "usage: agentmux-server (--serve|--daemon) [--socket PATH] [--data-dir PATH] [--config PATH]\n\
         \n\
         --serve   run the daemon in the foreground\n\
         --daemon  spawn a detached daemon and exit (TUI auto-start path)\n\
         \n\
         socket:    --socket > $AGENTMUX_SOCK > <data-dir>/agentmux.sock\n\
         data dir:  --data-dir > $AGENTMUX_DATA_DIR > $XDG_DATA_HOME/agentmux\n\
         \\                    (default ~/.local/share/agentmux)\n\
         config:    --config > $AGENTMUX_CONFIG > $XDG_CONFIG_HOME/agentmux/config.toml\n\
         \\                    (default ~/.config/agentmux/config.toml)"
    );
    exit(2)
}

fn parse_args() -> (Mode, Option<PathBuf>, Option<PathBuf>, Option<PathBuf>) {
    let mut mode = None;
    let mut socket = None;
    let mut data_dir = None;
    let mut config = None;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--serve" if mode.is_none() => mode = Some(Mode::Serve),
            "--daemon" if mode.is_none() => mode = Some(Mode::Daemon),
            "--socket" => socket = Some(PathBuf::from(args.next().unwrap_or_else(|| usage()))),
            "--data-dir" => data_dir = Some(PathBuf::from(args.next().unwrap_or_else(|| usage()))),
            "--config" => config = Some(PathBuf::from(args.next().unwrap_or_else(|| usage()))),
            "-h" | "--help" => usage(),
            _ => usage(),
        }
    }
    match mode {
        Some(mode) => (mode, socket, data_dir, config),
        None => usage(),
    }
}

#[tokio::main]
async fn main() {
    let (mode, socket, data_dir, config) = parse_args();
    let paths = ServerPaths::resolve(socket, data_dir, config);
    let code = match mode {
        Mode::Serve => serve(paths).await,
        Mode::Daemon => daemonize(&paths).await,
    };
    if code != 0 {
        exit(code);
    }
}

/// `--serve`: claim the data dir and socket, then build the daemon and
/// run it in the foreground until `server/shutdown` (or the socket going
/// away).
///
/// Order matters: building the daemon sweeps every live session in the
/// store to `Error`, so it only happens once this process provably owns
/// both the data dir (lock) and the socket (bind).
async fn serve(paths: ServerPaths) -> i32 {
    let _lock = match lock_data_dir(&paths.data_dir) {
        Ok(lock) => lock,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            eprintln!(
                "agentmux-server: another daemon is already using {}",
                paths.data_dir.display()
            );
            return 1;
        }
        Err(e) => {
            eprintln!(
                "agentmux-server: cannot lock {}: {e}",
                paths.data_dir.display()
            );
            return 1;
        }
    };
    let listener = match bind_unix_listener(&paths.socket_path).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!(
                "agentmux-server: another daemon is already serving {}",
                paths.socket_path.display()
            );
            return 1;
        }
        Err(e) => {
            eprintln!(
                "agentmux-server: cannot bind {}: {e}",
                paths.socket_path.display()
            );
            return 1;
        }
    };
    let daemon = match build_daemon(&paths) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("agentmux-server: init failed: {e:#}");
            return 1;
        }
    };
    eprintln!(
        "agentmux-server: listening on {}",
        paths.socket_path.display()
    );
    if let Err(e) = daemon.serve(listener).await {
        eprintln!("agentmux-server: serve failed: {e:#}");
        return 1;
    }
    0
}

/// `--daemon`: detached spawn of `self --serve` (daemonize-lite — see the
/// module docs). Prints the child pid and socket path for the caller.
///
/// The child's stderr goes to /dev/null, so init failures are silent —
/// after `spawn` we poll-connect to the socket for up to 3s (failing
/// early if the child has already exited) and only report success once
/// it is actually accepting connections.
async fn daemonize(paths: &ServerPaths) -> i32 {
    let exe = match env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("agentmux-server: cannot locate self executable: {e}");
            return 1;
        }
    };
    // If the socket already accepts, a daemon is live — spawning a child
    // would only make it fail its bind, and the readiness poll below
    // would false-positive on the existing socket anyway.
    if tokio::net::UnixStream::connect(&paths.socket_path)
        .await
        .is_ok()
    {
        println!(
            "agentmux-server: already running on {}",
            paths.socket_path.display()
        );
        return 0;
    }
    let mut cmd = Command::new(exe);
    cmd.arg("--serve")
        .arg("--socket")
        .arg(&paths.socket_path)
        .arg("--data-dir")
        .arg(&paths.data_dir)
        .arg("--config")
        .arg(&paths.config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // New process group: the daemon survives the TUI/terminal exiting.
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("agentmux-server: daemon spawn failed: {e}");
            return 1;
        }
    };
    let pid = child.id();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match tokio::net::UnixStream::connect(&paths.socket_path).await {
            Ok(_) => {
                println!(
                    "agentmux-server: daemonized (pid {pid}) on {}",
                    paths.socket_path.display()
                );
                return 0;
            }
            Err(e) => {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        eprintln!(
                            "agentmux-server: daemon exited during startup ({status}); \
                             socket {} never came up",
                            paths.socket_path.display()
                        );
                        return 1;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        eprintln!("agentmux-server: cannot check daemon status: {e}");
                        return 1;
                    }
                }
                if std::time::Instant::now() >= deadline {
                    eprintln!(
                        "agentmux-server: daemon (pid {pid}) did not start listening on \
                         {} within 3s: {e}",
                        paths.socket_path.display()
                    );
                    return 1;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
}
