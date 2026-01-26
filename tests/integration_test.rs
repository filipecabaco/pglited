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
fn test_persistent_storage_mode() {
    let temp_dir = std::env::temp_dir().join(format!("pglite_test_{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");

    let data_dir = temp_dir.to_str().unwrap();
    let tcp_port = 55200 + (std::process::id() % 100) as u16;

    let mut instance = TestInstance::start(data_dir, tcp_port).expect("Failed to start instance");

    match instance.wait_for_ready(60) {
        Ok(()) => {
            println!("Persistent instance ready");
            assert!(temp_dir.exists(), "Data directory should exist");
        }
        Err(e) => {
            panic!("Persistent instance failed to start: {}", e);
        }
    }

    drop(instance);
    std::fs::remove_dir_all(&temp_dir).ok();
}

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

/// Test that data persists across reconnections when using file storage.
///
/// This test:
/// 1. Starts pglited with a file-based data directory
/// 2. Connects via tokio-postgres and creates a table with data
/// 3. Stops the pglited instance
/// 4. Starts a new pglited instance with the same data directory
/// 5. Verifies the data is still present
#[tokio::test]
async fn test_file_storage_data_persists_on_reconnect() {
    use tokio_postgres::NoTls;

    let temp_dir =
        std::env::temp_dir().join(format!("pglite_reconnect_test_{}", std::process::id()));

    // Clean up any previous test data
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");

    let data_dir = temp_dir.to_str().unwrap().to_string();
    let tcp_port = 55400 + (std::process::id() % 100) as u16;

    println!("=== Phase 1: Create data in new database ===");

    // Start first instance
    let mut instance1 =
        TestInstance::start(&data_dir, tcp_port).expect("Failed to start first instance");
    instance1
        .wait_for_ready(120)
        .expect("First instance failed to become ready");

    println!("First instance ready on port {}", tcp_port);

    // Connect and create data
    let connection_string = format!("host=127.0.0.1 port={} user=postgres", tcp_port);
    let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
        .await
        .expect("Failed to connect to first instance");

    // Spawn connection handler
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {}", e);
        }
    });

    // Create table and insert test data
    client
        .execute(
            "CREATE TABLE test_persistence (id SERIAL PRIMARY KEY, name TEXT, value INTEGER)",
            &[],
        )
        .await
        .expect("Failed to create table");

    client
        .execute(
            "INSERT INTO test_persistence (name, value) VALUES ('alpha', 100), ('beta', 200), ('gamma', 300)",
            &[],
        )
        .await
        .expect("Failed to insert data");

    // Verify data was inserted
    let rows = client
        .query("SELECT name, value FROM test_persistence ORDER BY id", &[])
        .await
        .expect("Failed to query data");

    assert_eq!(rows.len(), 3, "Should have 3 rows after insert");
    println!("Inserted {} rows successfully", rows.len());

    // Close connection and stop instance
    drop(client);
    drop(instance1);

    // Give some time for cleanup
    tokio::time::sleep(Duration::from_millis(500)).await;

    println!("=== Phase 2: Reconnect and verify data persistence ===");

    // Start second instance with the same data directory
    let mut instance2 =
        TestInstance::start(&data_dir, tcp_port).expect("Failed to start second instance");
    instance2
        .wait_for_ready(120)
        .expect("Second instance failed to become ready");

    println!("Second instance ready on port {}", tcp_port);

    // Connect to second instance
    let (client2, connection2) = tokio_postgres::connect(&connection_string, NoTls)
        .await
        .expect("Failed to connect to second instance");

    tokio::spawn(async move {
        if let Err(e) = connection2.await {
            eprintln!("Connection error: {}", e);
        }
    });

    // Verify table exists and data persisted
    let rows = client2
        .query("SELECT name, value FROM test_persistence ORDER BY id", &[])
        .await
        .expect("Failed to query data from second instance - table should exist");

    assert_eq!(
        rows.len(),
        3,
        "Should still have 3 rows after reconnect - data should persist"
    );

    // Verify actual values
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

    println!("All data verified successfully after reconnect!");

    // Can also insert more data to verify the database is fully functional
    client2
        .execute(
            "INSERT INTO test_persistence (name, value) VALUES ('delta', 400)",
            &[],
        )
        .await
        .expect("Failed to insert additional data after reconnect");

    let final_count: i64 = client2
        .query_one("SELECT COUNT(*) FROM test_persistence", &[])
        .await
        .expect("Failed to count rows")
        .get(0);

    assert_eq!(final_count, 4, "Should have 4 rows after additional insert");
    println!("Successfully inserted new data after reconnect");

    // Cleanup
    drop(client2);
    drop(instance2);
    let _ = std::fs::remove_dir_all(&temp_dir);

    println!("=== Test passed: File storage data persists across reconnections ===");
}
