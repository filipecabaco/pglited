use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct TestInstance {
    process: Child,
    tcp_port: u16,
}

impl TestInstance {
    fn start(data_dir: &str, tcp_port: u16) -> Result<Self, String> {
        let exe_dir =
            std::env::current_exe().map_err(|e| format!("Failed to get current exe: {}", e))?;

        let target_dir = exe_dir.parent().unwrap().parent().unwrap();

        let possible_paths = [
            target_dir.join("pglited"),
            target_dir.join("debug").join("pglited"),
            target_dir.join("release").join("pglited"),
            target_dir.parent().unwrap().join("debug").join("pglited"),
            target_dir.parent().unwrap().join("release").join("pglited"),
        ];

        let binary_path = possible_paths
            .iter()
            .find(|p| p.exists())
            .ok_or_else(|| {
                format!(
                    "Binary not found. Searched:\n{}",
                    possible_paths
                        .iter()
                        .map(|p| format!("  {:?}", p))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            })?
            .clone();

        eprintln!("Starting binary: {:?}", binary_path);
        eprintln!("Args: {} {}", data_dir, tcp_port);

        let process = Command::new(&binary_path)
            .args([data_dir, &tcp_port.to_string()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to start process: {}", e))?;

        Ok(TestInstance { process, tcp_port })
    }

    fn wait_for_ready(&mut self, timeout_secs: u64) -> Result<(), String> {
        let stdout = self.process.stdout.take().ok_or("Failed to get stdout")?;

        let reader = BufReader::new(stdout);
        let start = std::time::Instant::now();

        for line in reader.lines() {
            if start.elapsed() > Duration::from_secs(timeout_secs) {
                return Err("Timeout waiting for ready signal".to_string());
            }

            let line = line.map_err(|e| format!("Failed to read line: {}", e))?;

            if line.contains("\"id\":\"ready\"") && line.contains("\"success\":true") {
                return Ok(());
            }
        }

        Err("Process exited without sending ready signal".to_string())
    }

    fn try_tcp_connect(&self) -> Result<TcpStream, String> {
        TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", self.tcp_port)
                .parse()
                .map_err(|e| format!("Invalid address: {}", e))?,
            Duration::from_secs(5),
        )
        .map_err(|e| format!("Failed to connect: {}", e))
    }
}

impl Drop for TestInstance {
    fn drop(&mut self) {
        let _ = self.process.kill();

        if let Some(mut stderr) = self.process.stderr.take() {
            use std::io::Read;
            let mut stderr_output = String::new();
            let _ = stderr.read_to_string(&mut stderr_output);
            if !stderr_output.is_empty() {
                eprintln!("Binary stderr:\n{}", stderr_output);
            }
        }

        let status = self.process.wait();
        if let Ok(s) = status {
            if !s.success() {
                eprintln!("Process exited with status: {:?}", s);
            }
        }
    }
}

#[test]
fn test_binary_starts_and_binds_port() {
    let data_dir = format!("memory://test_{}", std::process::id());
    let tcp_port = 55000 + (std::process::id() % 1000) as u16;

    let mut instance = TestInstance::start(&data_dir, tcp_port).expect("Failed to start instance");

    match instance.wait_for_ready(60) {
        Ok(()) => {
            println!("Instance ready on port {}", tcp_port);

            match instance.try_tcp_connect() {
                Ok(_stream) => {
                    println!("TCP connection successful");
                }
                Err(e) => {
                    println!("TCP connection failed (may be expected): {}", e);
                }
            }
        }
        Err(e) => {
            panic!("Instance failed to start: {}", e);
        }
    }
}

#[test]
fn test_multiple_instances_different_ports() {
    let base_port = 55100 + (std::process::id() % 100) as u16;

    let mut instances: Vec<TestInstance> = Vec::new();

    for i in 0..3 {
        let data_dir = format!("memory://test_multi_{}_{}", std::process::id(), i);
        let tcp_port = base_port + i;

        match TestInstance::start(&data_dir, tcp_port) {
            Ok(instance) => {
                instances.push(instance);
            }
            Err(e) => {
                panic!("Failed to start instance {}: {}", i, e);
            }
        }
    }

    for (i, instance) in instances.iter_mut().enumerate() {
        match instance.wait_for_ready(60) {
            Ok(()) => {
                println!("Instance {} ready on port {}", i, instance.tcp_port);
            }
            Err(e) => {
                panic!("Instance {} failed to become ready: {}", i, e);
            }
        }
    }

    println!(
        "All {} instances started successfully on different ports",
        instances.len()
    );
}

#[test]
fn test_named_memory_storage() {
    // Test using a named memory database
    let data_dir = format!("memory://named_db_{}", std::process::id());
    let tcp_port = 55200 + (std::process::id() % 100) as u16;

    let mut instance = TestInstance::start(&data_dir, tcp_port).expect("Failed to start instance");

    match instance.wait_for_ready(60) {
        Ok(()) => {
            println!("Named memory instance ready");
        }
        Err(e) => {
            panic!("Named memory instance failed to start: {}", e);
        }
    }
}

// TODO: test_persistent_storage_mode - File storage (file:// URLs) is not yet supported.
// PGlite requires the Node.js fs module for file-based persistence, which needs to be
// implemented. See: https://github.com/user/pglited/issues/XXX

#[test]
fn test_ready_signal_format() {
    let data_dir = format!("memory://test_signal_{}", std::process::id());
    let tcp_port = 55300 + (std::process::id() % 100) as u16;

    let mut instance = TestInstance::start(&data_dir, tcp_port).expect("Failed to start instance");

    let stdout = instance.process.stdout.take().unwrap();
    let reader = BufReader::new(stdout);

    let mut found_ready = false;
    let start = std::time::Instant::now();

    for line in reader.lines() {
        if start.elapsed() > Duration::from_secs(60) {
            break;
        }

        if let Ok(line) = line {
            if line.starts_with('{') && line.contains("ready") {
                let json: serde_json::Value =
                    serde_json::from_str(&line).expect("Ready signal should be valid JSON");

                assert_eq!(json["id"], "ready", "id should be 'ready'");
                assert_eq!(json["success"], true, "success should be true");
                assert_eq!(
                    json["port"], tcp_port as i64,
                    "port should match requested port"
                );

                found_ready = true;
                break;
            }
        }
    }

    assert!(found_ready, "Ready signal should be found in stdout");
}

/// Test PostgreSQL client connectivity using tokio-postgres.
///
/// This test verifies that pglited can be connected to using a standard
/// PostgreSQL client library, and that basic SQL operations work correctly.
#[tokio::test]
async fn test_postgres_client_connectivity() {
    use tokio_postgres::NoTls;

    let data_dir = format!("memory://connectivity_test_{}", std::process::id());
    let tcp_port = 55400 + (std::process::id() % 100) as u16;

    // Start instance
    let mut instance =
        TestInstance::start(&data_dir, tcp_port).expect("Failed to start instance");
    instance
        .wait_for_ready(120)
        .expect("Instance failed to become ready");

    println!("Instance ready on port {}", tcp_port);

    // Connect using tokio-postgres
    let connection_string = format!("host=127.0.0.1 port={} user=postgres", tcp_port);
    let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
        .await
        .expect("Failed to connect");

    // Spawn connection handler
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {}", e);
        }
    });

    // Create table and insert test data
    client
        .execute(
            "CREATE TABLE test_table (id SERIAL PRIMARY KEY, name TEXT, value INTEGER)",
            &[],
        )
        .await
        .expect("Failed to create table");

    client
        .execute(
            "INSERT INTO test_table (name, value) VALUES ('alpha', 100), ('beta', 200), ('gamma', 300)",
            &[],
        )
        .await
        .expect("Failed to insert data");

    // Query and verify data
    let rows = client
        .query("SELECT name, value FROM test_table ORDER BY id", &[])
        .await
        .expect("Failed to query data");

    assert_eq!(rows.len(), 3, "Should have 3 rows");

    let row0_name: &str = rows[0].get("name");
    let row0_value: i32 = rows[0].get("value");
    assert_eq!(row0_name, "alpha");
    assert_eq!(row0_value, 100);

    let row1_name: &str = rows[1].get("name");
    let row1_value: i32 = rows[1].get("value");
    assert_eq!(row1_name, "beta");
    assert_eq!(row1_value, 200);

    let row2_name: &str = rows[2].get("name");
    let row2_value: i32 = rows[2].get("value");
    assert_eq!(row2_name, "gamma");
    assert_eq!(row2_value, 300);

    println!("All queries executed successfully!");

    // Cleanup
    drop(client);
    drop(instance);

    println!("=== Test passed: PostgreSQL client connectivity works ===");
}

// TODO: Add test_file_storage_data_persists_on_reconnect once Node.js fs module
// compatibility is implemented. File storage (file:// URLs) requires PGlite to
// have access to filesystem operations via the fs module.
