use flate2::read::GzDecoder;
use std::fs;
use std::io::Read;
use std::path::Path;

const PGLITE_VERSION: &str = "0.5.4";

/// Extensions that used to ship inside the `@electric-sql/pglite` tarball and
/// now live in their own packages. Each one is laid out under
/// `dist/<name>/`, which is where its `index.js` resolves its own
/// `<name>.tar.gz` bundle from.
const EXTENSION_PACKAGES: &[(&str, &str, &str)] = &[
    // (directory name, npm package name, version)
    ("vector", "pglite-pgvector", "0.0.6"),
    ("pgtap", "pglite-pgtap", "0.0.6"),
    ("pg_ivm", "pglite-pg_ivm", "0.0.6"),
    ("pg_uuidv7", "pglite-pg_uuidv7", "0.0.6"),
    ("pg_hashids", "pglite-pg_hashids", "0.0.6"),
];

fn npm_tarball_url(package: &str, version: &str) -> String {
    format!(
        "https://registry.npmjs.org/@electric-sql/{0}/-/{0}-{1}.tgz",
        package, version
    )
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/js");
    println!("cargo:rerun-if-changed=assets/pglite_npm/dist/pgdata_seed.tar");

    fs::create_dir_all("assets").expect("Failed to create assets directory");
    ensure_npm_assets();
    ensure_extension_packages();
    report_pgdata_seed();
    println!("cargo:warning=All assets ready");
}

/// Downloads an npm tarball and unpacks its `package/<subdir>` into `dest`.
fn fetch_and_unpack(url: &str, strip_prefix: &str, dest: &Path) {
    println!("cargo:warning=Fetching: {}", url);

    let response = ureq::get(url)
        .call()
        .unwrap_or_else(|e| panic!("Failed to download {}: {}", url, e));

    let mut compressed_data = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut compressed_data)
        .unwrap_or_else(|e| panic!("Failed to read {}: {}", url, e));

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

        let relative = match path.strip_prefix(strip_prefix) {
            Ok(rel) => rel,
            Err(_) => continue,
        };

        // npm tarballs are third-party input.
        if relative
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            panic!("Refusing to unpack tar entry with '..': {:?}", path);
        }

        let dest_path = dest.join(relative);
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent).expect("Failed to create asset subdirectory");
        }

        entry
            .unpack(&dest_path)
            .unwrap_or_else(|e| panic!("Failed to unpack {:?}: {}", dest_path, e));
    }
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

    fetch_and_unpack(
        &npm_tarball_url("pglite", PGLITE_VERSION),
        "package/dist",
        dist_dir,
    );
}

fn ensure_extension_packages() {
    let dist_dir = Path::new("assets/pglite_npm/dist");

    for (name, package, version) in EXTENSION_PACKAGES {
        let ext_dir = dist_dir.join(name);
        if ext_dir.join("index.js").exists() {
            continue;
        }

        println!(
            "cargo:warning=Downloading extension {} ({}@{})...",
            name, package, version
        );
        fs::create_dir_all(&ext_dir).expect("Failed to create extension directory");
        fetch_and_unpack(&npm_tarball_url(package, version), "package/dist", &ext_dir);
    }
}

fn report_pgdata_seed() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let seed_path = Path::new(&manifest_dir).join("assets/pglite_npm/dist/pgdata_seed.tar");

    if seed_path.exists() {
        return;
    }

    println!("cargo:warning=pgdata_seed.tar is missing; file:// databases will fall back to a slow initdb");
    println!("cargo:warning=Generate it with: mise run seed");
}
