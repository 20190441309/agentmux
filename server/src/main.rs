//! `agentmux-server` binary — CLI plumbing only; the daemon itself lives
//! in `agentmux_server::rpc_server`.
//!
//! ```text
//! agentmux-server --serve   [--socket PATH] [--data-dir PATH] [--config PATH]
//! agentmux-server --daemon  [--socket PATH] [--data-dir PATH] [--config PATH]
//! agentmux-server --serve --web [--web-listen ADDR] [--web-expose local|tunnel]
//! agentmux-server --pair | --devices | --revoke DEVICE_ID
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

use agentmux_server::{
    bind_unix_listener, build_daemon, lock_data_dir, serve_web, web::run_tunnel, ServerPaths,
    WebOptions,
};

enum Mode {
    Serve,
    Daemon,
    Pair,
    Devices,
    Revoke(String),
}

#[derive(Default)]
struct WebCli {
    enabled: bool,
    listen: Option<String>,
    expose_tunnel: bool,
    insecure_http: bool,
    web_dir: Option<PathBuf>,
    allowed_origins: Vec<String>,
}

fn usage() -> ! {
    eprintln!(
        "usage: agentmux-server (--serve|--daemon) [--socket PATH] [--data-dir PATH] [--config PATH]\n\
         \n\
         --serve   run the daemon in the foreground\n\
         --daemon  spawn a detached daemon and exit (TUI auto-start path)\n\
         \n\
         web gateway (with --serve):\n\
         --web                    enable the browser gateway (default 127.0.0.1:8787)\n\
         --web-listen ADDR        listen address; non-loopback needs --insecure-http\n\
         --web-expose local|tunnel  tunnel runs `cloudflared` for a public https URL\n\
         --web-origin ORIGIN      extra allowed browser origin (repeatable)\n\
         --web-dir DIR            serve the UI from DIR instead of the embedded build\n\
         --insecure-http          allow plain HTTP on a non-loopback address\n\
         \n\
         device management (talks to the running daemon):\n\
         --pair                   print a one-time pairing link\n\
         --devices                list paired devices\n\
         --revoke ID              revoke a device\n\
         \n\
         socket:    --socket > $AGENTMUX_SOCK > <data-dir>/agentmux.sock\n\
         data dir:  --data-dir > $AGENTMUX_DATA_DIR > $XDG_DATA_HOME/agentmux\n\
         \\                    (default ~/.local/share/agentmux)\n\
         config:    --config > $AGENTMUX_CONFIG > $XDG_CONFIG_HOME/agentmux/config.toml\n\
         \\                    (default ~/.config/agentmux/config.toml)"
    );
    exit(2)
}

fn parse_args() -> (
    Mode,
    Option<PathBuf>,
    Option<PathBuf>,
    Option<PathBuf>,
    WebCli,
) {
    let mut mode = None;
    let mut socket = None;
    let mut data_dir = None;
    let mut config = None;
    let mut web = WebCli::default();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--serve" if mode.is_none() => mode = Some(Mode::Serve),
            "--daemon" if mode.is_none() => mode = Some(Mode::Daemon),
            "--pair" if mode.is_none() => mode = Some(Mode::Pair),
            "--devices" if mode.is_none() => mode = Some(Mode::Devices),
            "--revoke" if mode.is_none() => {
                mode = Some(Mode::Revoke(args.next().unwrap_or_else(|| usage())))
            }
            "--web" => web.enabled = true,
            "--web-listen" => {
                web.enabled = true;
                web.listen = Some(args.next().unwrap_or_else(|| usage()));
            }
            "--web-expose" => match args.next().as_deref() {
                Some("tunnel") => {
                    web.enabled = true;
                    web.expose_tunnel = true;
                }
                Some("local") => web.enabled = true,
                _ => usage(),
            },
            "--web-origin" => web
                .allowed_origins
                .push(args.next().unwrap_or_else(|| usage())),
            "--web-dir" => {
                web.web_dir = Some(PathBuf::from(args.next().unwrap_or_else(|| usage())))
            }
            "--insecure-http" => web.insecure_http = true,
            "--socket" => socket = Some(PathBuf::from(args.next().unwrap_or_else(|| usage()))),
            "--data-dir" => data_dir = Some(PathBuf::from(args.next().unwrap_or_else(|| usage()))),
            "--config" => config = Some(PathBuf::from(args.next().unwrap_or_else(|| usage()))),
            "-h" | "--help" => usage(),
            _ => usage(),
        }
    }
    match mode {
        Some(mode) => (mode, socket, data_dir, config, web),
        None => usage(),
    }
}

#[tokio::main]
async fn main() {
    let (mode, socket, data_dir, config, web) = parse_args();
    let paths = ServerPaths::resolve(socket, data_dir, config);
    let code = match mode {
        Mode::Serve => serve(paths, web).await,
        Mode::Daemon => daemonize(&paths).await,
        Mode::Pair => {
            local_rpc(&paths, "web/pair_create", serde_json::json!({}), |r| {
                let code = r["code"].as_str().unwrap_or_default();
                match r["url"].as_str() {
                    Some(url) => println!("{url}/#pair={code}"),
                    None => println!("pairing code: {code}  (open the web UI and enter it)"),
                }
                println!("valid for 5 minutes, single use");
            })
            .await
        }
        Mode::Devices => {
            local_rpc(&paths, "web/device_list", serde_json::json!({}), |r| {
                for d in r["devices"].as_array().into_iter().flatten() {
                    println!(
                        "{}  {}  last seen {}",
                        d["id"].as_str().unwrap_or("?"),
                        d["name"].as_str().unwrap_or("?"),
                        d["last_seen"].as_str().unwrap_or("?")
                    );
                }
            })
            .await
        }
        Mode::Revoke(id) => {
            local_rpc(
                &paths,
                "web/device_revoke",
                serde_json::json!({ "device_id": id }),
                |r| {
                    println!("revoked: {}", r["revoked"]);
                },
            )
            .await
        }
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
async fn serve(paths: ServerPaths, web: WebCli) -> i32 {
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
    if web.enabled {
        if let Err(code) = start_web(&daemon, web).await {
            return code;
        }
    }
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

/// Bind the browser gateway (and optionally the public tunnel) in the
/// background. Refuses insecure exposure instead of starting quietly.
async fn start_web(
    daemon: &std::sync::Arc<agentmux_server::Daemon>,
    web: WebCli,
) -> Result<(), i32> {
    let listen = web
        .listen
        .clone()
        .unwrap_or_else(|| "127.0.0.1:8787".into());
    let addr: std::net::SocketAddr = match listen.parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("agentmux-server: bad --web-listen {listen:?}: {e}");
            return Err(2);
        }
    };
    if !addr.ip().is_loopback() && !web.insecure_http {
        eprintln!(
            "agentmux-server: refusing to serve plain HTTP on {addr}. Use --web-expose tunnel \
             (public https URL), put a TLS proxy in front and listen on 127.0.0.1, or pass \
             --insecure-http to accept the risk."
        );
        return Err(2);
    }
    if web.expose_tunnel && !addr.ip().is_loopback() {
        eprintln!("agentmux-server: --web-expose tunnel requires a loopback --web-listen");
        return Err(2);
    }
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("agentmux-server: cannot bind web gateway on {addr}: {e}");
            return Err(1);
        }
    };
    let port = listener
        .local_addr()
        .map(|a| a.port())
        .unwrap_or(addr.port());
    let opts = WebOptions {
        web_dir: web.web_dir,
        allowed_origins: web.allowed_origins,
    };
    let d = daemon.clone();
    tokio::spawn(async move {
        if let Err(e) = serve_web(d, listener, opts).await {
            eprintln!("agentmux-server: web gateway stopped: {e}");
        }
    });
    if web.expose_tunnel {
        eprintln!("agentmux-server: starting tunnel…");
        tokio::spawn(run_tunnel(daemon.clone(), port));
    } else {
        agentmux_server::web::announce(daemon, &format!("http://{addr}"));
    }
    Ok(())
}

/// One-shot request to the running daemon over the unix socket.
async fn local_rpc(
    paths: &ServerPaths,
    method: &str,
    params: serde_json::Value,
    show: impl FnOnce(&serde_json::Value),
) -> i32 {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let stream = match tokio::net::UnixStream::connect(&paths.socket_path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "agentmux-server: no daemon on {} ({e}); start one with --serve --web",
                paths.socket_path.display()
            );
            return 1;
        }
    };
    let (r, mut w) = stream.into_split();
    let req = serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
    if w.write_all(format!("{req}\n").as_bytes()).await.is_err() {
        return 1;
    }
    let mut line = String::new();
    if BufReader::new(r).read_line(&mut line).await.is_err() {
        return 1;
    }
    let resp: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
    match resp.get("result") {
        Some(result) => {
            show(result);
            0
        }
        None => {
            eprintln!(
                "agentmux-server: request failed: {}",
                resp["error"]["message"]
            );
            1
        }
    }
}
