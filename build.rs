use std::fs::{self, File};
use std::io::{Read, Write as IoWrite};
use std::path::{Path, PathBuf};

use wasmtime::{Config, Engine, Linker, Module, Store, Val};
use wasmtime_wasi::p1::WasiP1Ctx;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

const PGLITE_WASI_URL: &str = "https://raw.githubusercontent.com/kshcherban/pglite-rust-bindings/main/assets/pglite-wasi.tar.xz";

fn main() {
    println!("cargo:rerun-if-changed=assets/pglite.wasi");

    fs::create_dir_all("assets").expect("Failed to create assets directory");

    let wasi_path = Path::new("assets/pglite.wasi");
    let cwasm_path = Path::new("assets/pglite.cwasm");
    let prefix_path = Path::new("assets/prefix.tar.zst");
    let pgdata_seed_path = Path::new("assets/pgdata_seed.tar.zst");

    let mut prefix_dir: Option<PathBuf> = None;

    if !wasi_path.exists() || !prefix_path.exists() {
        println!("cargo:warning=Downloading pglite WASI build from GitHub...");
        prefix_dir = Some(download_and_extract_wasi());
    }

    if should_rebuild_cwasm(wasi_path, cwasm_path) {
        println!("cargo:warning=Compiling pglite.cwasm for target architecture...");
        if let Err(e) = compile_cwasm(wasi_path, cwasm_path) {
            panic!("Failed to compile cwasm: {}", e);
        }
        println!("cargo:warning=Successfully compiled pglite.cwasm");
    }

    if should_rebuild_pgdata_seed(cwasm_path, pgdata_seed_path) {
        println!("cargo:warning=Generating pgdata_seed.tar.zst (this takes ~10s)...");

        let prefix = prefix_dir.unwrap_or_else(extract_prefix_to_temp);

        if let Err(e) = generate_pgdata_seed(cwasm_path, &prefix, pgdata_seed_path) {
            panic!("Failed to generate pgdata_seed: {}", e);
        }

        let _ = fs::remove_dir_all(&prefix);

        println!("cargo:warning=Successfully generated pgdata_seed.tar.zst");
    } else if let Some(ref prefix) = prefix_dir {
        let _ = fs::remove_dir_all(prefix);
    }

    println!("cargo:warning=All assets ready");
}

fn should_rebuild_cwasm(wasi_path: &Path, cwasm_path: &Path) -> bool {
    if !cwasm_path.exists() {
        return true;
    }

    let wasi_modified = fs::metadata(wasi_path).and_then(|m| m.modified()).ok();
    let cwasm_modified = fs::metadata(cwasm_path).and_then(|m| m.modified()).ok();

    match (wasi_modified, cwasm_modified) {
        (Some(wasi_time), Some(cwasm_time)) => wasi_time > cwasm_time,
        _ => true,
    }
}

fn should_rebuild_pgdata_seed(cwasm_path: &Path, pgdata_seed_path: &Path) -> bool {
    if !pgdata_seed_path.exists() {
        return true;
    }

    let cwasm_modified = fs::metadata(cwasm_path).and_then(|m| m.modified()).ok();
    let seed_modified = fs::metadata(pgdata_seed_path)
        .and_then(|m| m.modified())
        .ok();

    match (cwasm_modified, seed_modified) {
        (Some(cwasm_time), Some(seed_time)) => cwasm_time > seed_time,
        _ => true,
    }
}

fn download_and_extract_wasi() -> PathBuf {
    println!("cargo:warning=Fetching: {}", PGLITE_WASI_URL);

    let response = ureq::get(PGLITE_WASI_URL)
        .call()
        .unwrap_or_else(|e| panic!("Failed to download pglite-wasi.tar.xz: {}", e));

    let mut compressed_data = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut compressed_data)
        .expect("Failed to read response");

    println!(
        "cargo:warning=Downloaded {:.2} MB, extracting...",
        compressed_data.len() as f64 / (1024.0 * 1024.0)
    );

    let xz_decoder = xz2::read::XzDecoder::new(compressed_data.as_slice());
    let mut archive = tar::Archive::new(xz_decoder);

    let temp_dir = std::env::temp_dir().join(format!("pglite_extract_{}", std::process::id()));
    fs::create_dir_all(&temp_dir).expect("Failed to create temp directory");

    archive
        .unpack(&temp_dir)
        .expect("Failed to extract pglite-wasi.tar.xz");

    let wasi_src = temp_dir.join("tmp/pglite/bin/pglite.wasi");
    if wasi_src.exists() {
        fs::copy(&wasi_src, "assets/pglite.wasi").expect("Failed to copy pglite.wasi");
        println!("cargo:warning=Extracted pglite.wasi");
    } else {
        panic!("pglite.wasi not found in archive at {:?}", wasi_src);
    }

    let prefix_src = temp_dir.join("tmp/pglite");
    if prefix_src.exists() {
        create_prefix_tarball(&prefix_src, Path::new("assets/prefix.tar.zst"))
            .expect("Failed to create prefix tarball");
        println!("cargo:warning=Created prefix.tar.zst");
    }

    temp_dir
}

fn extract_prefix_to_temp() -> PathBuf {
    let prefix_tarball = Path::new("assets/prefix.tar.zst");
    let temp_dir = std::env::temp_dir().join(format!("pglite_prefix_{}", std::process::id()));
    fs::create_dir_all(&temp_dir).expect("Failed to create temp directory");

    let file = fs::File::open(prefix_tarball).expect("Failed to open prefix.tar.zst");
    let decoder = zstd::stream::Decoder::new(std::io::BufReader::new(file))
        .expect("Failed to create zstd decoder");
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(&temp_dir)
        .expect("Failed to extract prefix.tar.zst");

    temp_dir
}

fn create_prefix_tarball(prefix_dir: &Path, output: &Path) -> Result<(), String> {
    let mut tar_data = Vec::new();

    {
        let mut builder = tar::Builder::new(&mut tar_data);

        for entry in ["password", "lib", "share"].iter() {
            let src_path = prefix_dir.join(entry);
            if src_path.exists() {
                let archive_path = format!("tmp/pglite/{}", entry);
                if src_path.is_dir() {
                    builder
                        .append_dir_all(&archive_path, &src_path)
                        .map_err(|e| format!("Failed to add {} to tarball: {}", entry, e))?;
                } else {
                    let mut file = File::open(&src_path)
                        .map_err(|e| format!("Failed to open {}: {}", entry, e))?;
                    builder
                        .append_file(&archive_path, &mut file)
                        .map_err(|e| format!("Failed to add {} to tarball: {}", entry, e))?;
                }
            }
        }

        let mut header = tar::Header::new_gnu();
        header
            .set_path("tmp/pglite/bin/")
            .map_err(|e| e.to_string())?;
        header.set_size(0);
        header.set_mode(0o755);
        header.set_entry_type(tar::EntryType::Directory);
        header.set_cksum();
        builder
            .append(&header, std::io::empty())
            .map_err(|e| format!("Failed to add bin directory: {}", e))?;

        builder
            .finish()
            .map_err(|e| format!("Failed to finish tarball: {}", e))?;
    }

    let compressed = zstd::encode_all(tar_data.as_slice(), 3)
        .map_err(|e| format!("Failed to compress tarball: {}", e))?;

    fs::write(output, &compressed).map_err(|e| format!("Failed to write tarball: {}", e))?;

    Ok(())
}

fn compile_cwasm(wasi_path: &Path, cwasm_path: &Path) -> Result<(), String> {
    let mut config = Config::new();
    config.memory_init_cow(true);
    config.table_lazy_init(true);
    config.memory_guaranteed_dense_image_size(64 * 1024 * 1024);

    let engine = Engine::new(&config).map_err(|e| format!("Engine creation failed: {}", e))?;

    let module = Module::from_file(&engine, wasi_path)
        .map_err(|e| format!("Failed to load wasi module: {}", e))?;

    let bytes = module
        .serialize()
        .map_err(|e| format!("Failed to serialize module: {}", e))?;

    fs::write(cwasm_path, &bytes).map_err(|e| format!("Failed to write cwasm: {}", e))?;

    println!(
        "cargo:warning=Compiled cwasm: {} bytes ({:.2} MB)",
        bytes.len(),
        bytes.len() as f64 / (1024.0 * 1024.0)
    );

    Ok(())
}

fn generate_pgdata_seed(cwasm_path: &Path, prefix_dir: &Path, output: &Path) -> Result<(), String> {
    let pgdata_dir = std::env::temp_dir().join(format!("pglite_pgdata_{}", std::process::id()));
    fs::create_dir_all(&pgdata_dir).map_err(|e| format!("Failed to create pgdata dir: {}", e))?;

    println!("cargo:warning=  PGDATA location: {:?}", pgdata_dir);
    println!("cargo:warning=  Prefix location: {:?}", prefix_dir);

    let tmp_dir = prefix_dir.join("tmp");
    if !tmp_dir.exists() {
        fs::create_dir_all(&tmp_dir).map_err(|e| format!("Failed to create tmp dir: {}", e))?;
    }

    let mut config = Config::new();
    config.memory_init_cow(true);
    config.table_lazy_init(true);
    config.memory_guaranteed_dense_image_size(64 * 1024 * 1024);

    let engine = Engine::new(&config).map_err(|e| format!("Engine creation failed: {}", e))?;

    println!("cargo:warning=  Loading cwasm module...");
    let cwasm_bytes = fs::read(cwasm_path).map_err(|e| format!("Failed to read cwasm: {}", e))?;
    let module = unsafe {
        Module::deserialize(&engine, &cwasm_bytes)
            .map_err(|e| format!("Failed to deserialize cwasm: {}", e))?
    };

    let pgdata_str = pgdata_dir.to_str().ok_or("PGDATA path not UTF-8")?;

    let mut wasi_builder = WasiCtxBuilder::new();
    wasi_builder
        .inherit_stdio()
        .env("PGCLIENTENCODING", "UTF8")
        .env("REPL", "N")
        .env("LC_CTYPE", "en_US.UTF-8")
        .env("TZ", "UTC")
        .env("PGTZ", "UTC")
        .env("PGDATABASE", "template1")
        .env("PG_COLOR", "always")
        .env("PGUSER", "postgres")
        .env("PGDATA", pgdata_str)
        .env("PREFIX", "/tmp/pglite")
        .env("PGSYSCONFDIR", "/tmp/pglite");

    wasi_builder
        .preopened_dir(&tmp_dir, "/tmp", DirPerms::all(), FilePerms::all())
        .map_err(|e| format!("Failed to preopen tmp: {}", e))?;

    wasi_builder
        .preopened_dir(&pgdata_dir, pgdata_str, DirPerms::all(), FilePerms::all())
        .map_err(|e| format!("Failed to preopen pgdata: {}", e))?;

    wasi_builder
        .preopened_dir("/dev", "/dev", DirPerms::READ, FilePerms::READ)
        .map_err(|e| format!("Failed to preopen /dev: {}", e))?;

    let wasi = wasi_builder.build_p1();
    let mut store = Store::new(&engine, wasi);
    let mut linker = Linker::new(&engine);

    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |s: &mut WasiP1Ctx| s)
        .map_err(|e| format!("Failed to add WASI to linker: {}", e))?;

    println!("cargo:warning=  Instantiating module...");
    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|e| format!("Failed to instantiate module: {}", e))?;

    println!("cargo:warning=  Running _start...");
    if let Some(start_fn) = instance.get_func(&mut store, "_start") {
        start_fn
            .call(&mut store, &[], &mut [])
            .map_err(|e| format!("_start failed: {}", e))?;
    }

    println!("cargo:warning=  Running pgl_initdb (this is the slow part)...");
    if let Some(initdb) = instance.get_func(&mut store, "pgl_initdb") {
        let mut results = [Val::I32(0)];
        initdb
            .call(&mut store, &[], &mut results)
            .map_err(|e| format!("pgl_initdb failed: {}", e))?;
    }

    println!("cargo:warning=  Running pgl_backend...");
    if let Some(backend) = instance.get_func(&mut store, "pgl_backend") {
        backend
            .call(&mut store, &[], &mut [])
            .map_err(|e| format!("pgl_backend failed: {}", e))?;
    }

    println!("cargo:warning=  Running pgl_shutdown...");
    if let Some(shutdown) = instance.get_func(&mut store, "pgl_shutdown") {
        shutdown
            .call(&mut store, &[], &mut [])
            .map_err(|e| format!("pgl_shutdown failed: {}", e))?;
    }

    drop(store);
    std::thread::sleep(std::time::Duration::from_millis(100));

    if !pgdata_dir.exists()
        || fs::read_dir(&pgdata_dir)
            .map_err(|e| e.to_string())?
            .next()
            .is_none()
    {
        return Err("PGDATA directory is empty after initialization".to_string());
    }

    println!("cargo:warning=  Creating tarball...");
    let tar_data = create_pgdata_tarball(&pgdata_dir)?;

    println!("cargo:warning=  Compressing with zstd...");
    let compressed = zstd::encode_all(tar_data.as_slice(), 3)
        .map_err(|e| format!("Failed to compress: {}", e))?;

    fs::write(output, &compressed).map_err(|e| format!("Failed to write output: {}", e))?;

    println!(
        "cargo:warning=  pgdata_seed size: {:.2} MB",
        compressed.len() as f64 / (1024.0 * 1024.0)
    );

    let _ = fs::remove_dir_all(&pgdata_dir);

    Ok(())
}

fn create_pgdata_tarball(dir: &Path) -> Result<Vec<u8>, String> {
    let mut tar_data = Vec::new();

    {
        let mut builder = tar::Builder::new(&mut tar_data);
        add_dir_to_tar(&mut builder, dir, Path::new(""))?;
        builder
            .finish()
            .map_err(|e| format!("Failed to finish tarball: {}", e))?;
    }

    Ok(tar_data)
}

fn add_dir_to_tar<W: IoWrite>(
    builder: &mut tar::Builder<W>,
    base_dir: &Path,
    relative_path: &Path,
) -> Result<(), String> {
    let current_dir = base_dir.join(relative_path);

    for entry in fs::read_dir(&current_dir).map_err(|e| format!("Failed to read dir: {}", e))? {
        let entry = entry.map_err(|e| format!("Failed to read entry: {}", e))?;
        let path = entry.path();
        let name = entry.file_name();
        let entry_relative = relative_path.join(&name);

        if path.is_dir() {
            let mut header = tar::Header::new_gnu();
            header
                .set_path(&entry_relative)
                .map_err(|e| format!("Failed to set path: {}", e))?;
            header.set_size(0);
            header.set_mode(0o755);
            header.set_entry_type(tar::EntryType::Directory);
            header.set_cksum();
            builder
                .append(&header, std::io::empty())
                .map_err(|e| format!("Failed to append dir: {}", e))?;

            add_dir_to_tar(builder, base_dir, &entry_relative)?;
        } else {
            let data = fs::read(&path).map_err(|e| format!("Failed to read file: {}", e))?;
            let mut header = tar::Header::new_gnu();
            header
                .set_path(&entry_relative)
                .map_err(|e| format!("Failed to set path: {}", e))?;
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder
                .append(&header, data.as_slice())
                .map_err(|e| format!("Failed to append file: {}", e))?;
        }
    }

    Ok(())
}
