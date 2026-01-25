//! PGlite Port - QuickJS PGlite runtime
//!
//! Usage: pglited <data_dir> <tcp_port> [--multiplexer <mode>]
//!
//! Arguments:
//!   data_dir - Directory for PostgreSQL data or memory://
//!   tcp_port - TCP port to listen on for PostgreSQL connections
//!
//! Environment:
//!   PGLITE_DEBUG=1 - Enable verbose debug output

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use pglited::{AsyncPgliteExecutor, PgliteConfig, PgliteRuntime};
use serde_json::json;
use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

static DEBUG_ENABLED: Lazy<bool> = Lazy::new(|| {
    env::var("PGLITE_DEBUG")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false)
});

macro_rules! debug_log {
    ($($arg:tt)*) => {
        if *DEBUG_ENABLED {
            eprintln!($($arg)*);
        }
    };
}

enum Command {
    Serve(ServeArgs),
    DumpDataDir { output_path: String },
}

struct ServeArgs {
    data_dir: String,
    tcp_port: u16,
    multiplexer_mode: Option<String>,
}

impl Command {
    fn parse() -> Result<Self> {
        let args: Vec<String> = env::args().collect();

        if args.len() >= 2 && args[1] == "--dump-datadir" {
            if args.len() < 3 {
                eprintln!("Usage: {} --dump-datadir <output_path>", args[0]);
                std::process::exit(1);
            }
            return Ok(Command::DumpDataDir {
                output_path: args[2].clone(),
            });
        }

        if args.len() < 3 {
            Self::print_usage(&args[0]);
            std::process::exit(1);
        }

        let data_dir = args[1].clone();
        let tcp_port: u16 = args[2]
            .parse()
            .context("tcp_port must be a valid port number (1-65535)")?;
        let mut multiplexer_mode: Option<String> = None;

        let mut i = 3;
        while i < args.len() {
            match args[i].as_str() {
                "--multiplexer" => {
                    if i + 1 < args.len() {
                        multiplexer_mode = Some(args[i + 1].clone());
                        i += 2;
                    } else {
                        eprintln!("Error: --multiplexer requires a mode argument (e.g., queue)");
                        std::process::exit(1);
                    }
                }
                arg if arg.starts_with("--") => {
                    eprintln!("Unknown argument: {}", arg);
                    std::process::exit(1);
                }
                _ => {
                    eprintln!("Unknown argument: {}", args[i]);
                    std::process::exit(1);
                }
            }
        }

        Ok(Command::Serve(ServeArgs {
            data_dir,
            tcp_port,
            multiplexer_mode,
        }))
    }

    fn print_usage(program_name: &str) {
        eprintln!(
            "Usage: {} <data_dir> <tcp_port> [--multiplexer <mode>]",
            program_name
        );
        eprintln!("       {} --dump-datadir <output_path>", program_name);
        eprintln!();
        eprintln!("Commands:");
        eprintln!("  --dump-datadir <path>    - Dump initialized PostgreSQL data directory to a tar.gz file");
        eprintln!();
        eprintln!("Arguments:");
        eprintln!("  data_dir         - Directory for PostgreSQL data");
        eprintln!("  tcp_port         - TCP port for PostgreSQL connections");
        eprintln!();
        eprintln!("Options:");
        eprintln!("  --multiplexer <mode>     - Enable connection multiplexer (mode: queue)");
        eprintln!();
        eprintln!("Examples:");
        eprintln!("  {} memory:// 5432", program_name);
        eprintln!("  {} --dump-datadir pgdata_seed.tar.gz", program_name);
    }
}

impl ServeArgs {
    fn into_config(self) -> PgliteConfig {
        PgliteConfig {
            data_dir: self.data_dir,
            tcp_port: self.tcp_port,
        }
    }
}

fn setup_signal_handlers() {
    #[cfg(unix)]
    {
        use std::io::Read;

        std::thread::spawn(|| {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 1];

            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => {
                        debug_log!("[SIGNAL] stdin closed (parent died), shutting down");
                        SHUTDOWN.store(true, Ordering::SeqCst);
                        break;
                    }
                    Err(_) => {
                        debug_log!("[SIGNAL] stdin error, shutting down");
                        SHUTDOWN.store(true, Ordering::SeqCst);
                        break;
                    }
                    _ => {}
                }
            }
        });
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    setup_signal_handlers();

    let command = Command::parse()?;

    match command {
        Command::DumpDataDir { output_path } => {
            return dump_datadir_command(&output_path).await;
        }
        Command::Serve(args) => {
            return serve_command(args).await;
        }
    }
}

async fn dump_datadir_command(output_path: &str) -> Result<()> {
    use std::fs::File;
    use std::io::Write;

    eprintln!("Initializing PGlite to generate pgdata seed...");

    let config = PgliteConfig {
        data_dir: "memory://".to_string(),
        tcp_port: 0,
    };

    let runtime = tokio::task::spawn_blocking(move || -> Result<PgliteRuntime> {
        let mut runtime = PgliteRuntime::new(config)?;
        runtime.init_postgres()?;
        Ok(runtime)
    })
    .await??;

    eprintln!("Dumping data directory...");
    let data = runtime.dump_data_dir()?;

    let mut file = File::create(output_path)?;
    file.write_all(&data)?;

    let size_mb = data.len() as f64 / 1024.0 / 1024.0;
    eprintln!("Generated {} ({:.2} MB)", output_path, size_mb);

    Ok(())
}

async fn serve_command(args: ServeArgs) -> Result<()> {
    debug_log!("=== PGlite Wasmtime Port ===");
    debug_log!("Data Directory: {:?}", args.data_dir);
    debug_log!("TCP Port: {}", args.tcp_port);
    if let Some(ref mode) = args.multiplexer_mode {
        debug_log!("Multiplexer Mode: {}", mode);
    }
    debug_log!("Process ID: {}", std::process::id());

    let tcp_port = args.tcp_port;
    let multiplexer_mode = args.multiplexer_mode.clone();
    let config = args.into_config();

    debug_log!("\n=== Step 1: Creating Runtime ===");

    let runtime = tokio::task::spawn_blocking(move || -> Result<Arc<PgliteRuntime>> {
        let mut runtime = PgliteRuntime::new(config)?;

        debug_log!("✓ Runtime created (PGlite JS)");
        debug_log!("  Data dir: {}", runtime.data_dir);

        debug_log!("\n=== Step 2: Initializing PostgreSQL ===");
        runtime.init_postgres()?;
        debug_log!("✓ PostgreSQL initialized");

        Ok(Arc::new(runtime))
    })
    .await;

    let runtime = match runtime {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            eprintln!("PostgreSQL initialization failed: {:?}", e);
            println!(
                "{}",
                json!({"id": "ready", "success": false, "error": format!("{:?}", e)})
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Task join failed: {:?}", e);
            println!(
                "{}",
                json!({"id": "ready", "success": false, "error": format!("{:?}", e)})
            );
            std::process::exit(1);
        }
    };

    debug_log!("\n=== Step 3: Preparing TCP Socket ===");
    debug_log!("  Will bind to 127.0.0.1:{}", tcp_port);

    debug_log!("\n=== Step 4: Ready ===");
    let ready_json = if let Some(ref mode) = multiplexer_mode {
        json!({"id": "ready", "success": true, "port": tcp_port, "multiplexer": mode})
    } else {
        json!({"id": "ready", "success": true, "port": tcp_port})
    };
    println!("{}", ready_json);
    debug_log!("✓ Ready signal sent to Elixir");

    let executor = Arc::new(AsyncPgliteExecutor::new(Arc::clone(&runtime)));

    debug_log!("\n=== Step 5: Accepting Connections ===");

    let tokio_listener = tokio::net::TcpListener::bind(("127.0.0.1", tcp_port))
        .await
        .context("Failed to bind Tokio TCP listener")?;

    debug_log!("✓ Tokio TCP listener bound to 127.0.0.1:{}", tcp_port);

    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            debug_log!("[SHUTDOWN] Received shutdown signal, exiting accept loop");
            break;
        }

        tokio::select! {
            biased;

            result = tokio_listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        debug_log!("New connection from {:?}", addr);

                        let executor_clone = Arc::clone(&executor);

                        tokio::spawn(async move {
                            if let Err(e) = pglited::handle_connection_async(stream, executor_clone).await {
                                debug_log!("Connection error from {:?}: {:?}", addr, e);
                            } else {
                                debug_log!("Client {:?} disconnected", addr);
                            }
                        });
                    }
                    Err(e) => {
                        debug_log!("Accept error: {:?}", e);
                    }
                }
            }

            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                continue;
            }
        }
    }

    drop(runtime);
    debug_log!("[SHUTDOWN] Clean exit");
    Ok(())
}
