use flate2::read::GzDecoder;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::Command;

const PGLITE_VERSION: &str = "0.3.15";
const PGLITE_NPM_TARBALL: &str =
    "https://registry.npmjs.org/@electric-sql/pglite/-/pglite-0.3.15.tgz";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    fs::create_dir_all("assets").expect("Failed to create assets directory");
    ensure_npm_assets();
    ensure_pgdata_seed();
    println!("cargo:warning=All assets ready");
}

fn ensure_npm_assets() {
    let dist_dir = Path::new("assets/pglite_npm/dist");
    let index_path = dist_dir.join("index.js");
    if index_path.exists() {
        return;
    }

    fs::create_dir_all(dist_dir).expect("Failed to create npm assets directory");

    println!(
        "cargo:warning=Downloading pglite npm assets ({})...",
        PGLITE_VERSION
    );
    println!("cargo:warning=Fetching: {}", PGLITE_NPM_TARBALL);

    let response = ureq::get(PGLITE_NPM_TARBALL)
        .call()
        .unwrap_or_else(|e| panic!("Failed to download pglite npm tarball: {}", e));

    let mut compressed_data = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut compressed_data)
        .expect("Failed to read pglite npm tarball");

    println!(
        "cargo:warning=Downloaded {:.2} MB, extracting...",
        compressed_data.len() as f64 / (1024.0 * 1024.0)
    );

    let gz_decoder = GzDecoder::new(compressed_data.as_slice());
    let mut archive = tar::Archive::new(gz_decoder);

    for entry in archive.entries().expect("Failed to read npm tar entries") {
        let mut entry = entry.expect("Failed to read npm tar entry");
        let path = entry
            .path()
            .expect("Failed to read npm tar path")
            .to_path_buf();

        let relative = match path.strip_prefix("package/dist") {
            Ok(rel) => rel,
            Err(_) => continue,
        };

        let dest_path = dist_dir.join(relative);
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent).expect("Failed to create asset subdirectory");
        }

        entry
            .unpack(&dest_path)
            .unwrap_or_else(|e| panic!("Failed to unpack {:?}: {}", dest_path, e));
    }
}

fn ensure_pgdata_seed() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let seed_path = Path::new(&manifest_dir).join("assets/pglite_npm/dist/pgdata_seed.tar");
    if seed_path.exists() {
        println!("cargo:warning=pgdata_seed.tar already exists");
        return;
    }

    println!("cargo:warning=Generating pgdata_seed.tar for faster startup...");

    let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());

    let binary_name = if cfg!(windows) {
        "pglited.exe"
    } else {
        "pglited"
    };
    let binary_path = Path::new(&target_dir).join(&profile).join(binary_name);

    if !binary_path.exists() {
        println!(
            "cargo:warning=pglited binary not found at {:?}, skipping pgdata_seed generation",
            binary_path
        );
        println!("cargo:warning=Build once, then rebuild to generate pgdata_seed");
        return;
    }

    println!(
        "cargo:warning=Using pglited binary at {:?} to generate seed",
        binary_path
    );

    match Command::new(&binary_path)
        .arg("--dump-datadir")
        .arg(&seed_path)
        .current_dir(&manifest_dir)
        .output()
    {
        Ok(output) => {
            if output.status.success() {
                if seed_path.exists() {
                    let size = fs::metadata(seed_path)
                        .map(|m| m.len() as f64 / 1024.0 / 1024.0)
                        .unwrap_or(0.0);
                    println!(
                        "cargo:warning=Generated pgdata_seed.tar ({:.2} MB)",
                        size
                    );
                } else {
                    println!("cargo:warning=pgdata_seed generation completed but file not found");
                }
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                println!(
                    "cargo:warning=pgdata_seed generation failed (optional): {}",
                    stderr.lines().next().unwrap_or("unknown error")
                );
            }
        }
        Err(e) => {
            println!(
                "cargo:warning=Failed to run pglited binary: {}",
                e
            );
            println!("cargo:warning=Build once, then rebuild to generate pgdata_seed");
        }
    }
}
