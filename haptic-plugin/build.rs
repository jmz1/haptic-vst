use std::fs;
use std::path::{Path, PathBuf};

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn main() {
    let crate_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace_dir = crate_dir.parent().unwrap();
    let mut files = vec![
        crate_dir.join("build.rs"),
        crate_dir.join("Cargo.toml"),
        workspace_dir.join("Cargo.toml"),
        workspace_dir.join("Cargo.lock"),
        workspace_dir.join("haptic-protocol/Cargo.toml"),
    ];
    collect_files(&crate_dir.join("src"), &mut files);
    collect_files(&workspace_dir.join("haptic-protocol/src"), &mut files);
    files.sort();

    // Hash the client and wire-schema source content directly. Unlike a bare
    // Git revision this changes for successive dirty-worktree builds, while
    // remaining stable when identical source is rebuilt.
    let mut hash = FNV_OFFSET_BASIS;
    for path in &files {
        println!("cargo:rerun-if-changed={}", path.display());
        if let Ok(relative) = path.strip_prefix(workspace_dir) {
            hash_bytes(&mut hash, relative.to_string_lossy().as_bytes());
        }
        hash_bytes(&mut hash, &[0]);
        hash_bytes(
            &mut hash,
            &fs::read(path).unwrap_or_else(|error| {
                panic!(
                    "failed to read build-hash input {}: {error}",
                    path.display()
                )
            }),
        );
        hash_bytes(&mut hash, &[0xff]);
    }

    println!("cargo:rustc-env=HAPTIC_BUILD_HASH={hash:016x}");
}

fn collect_files(directory: &Path, output: &mut Vec<PathBuf>) {
    println!("cargo:rerun-if-changed={}", directory.display());
    let mut entries: Vec<_> = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", directory.display()))
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_files(&path, output);
        } else if path.is_file() {
            output.push(path);
        }
    }
}

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}
