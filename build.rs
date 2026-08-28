use std::{env, fs, path::PathBuf};

use sha2::{Digest, Sha256};

fn main() {
    let target = env::var("TARGET").expect("Cargo did not set TARGET");
    let source = PathBuf::from("browser-runtime").join(format!("{target}.tar.gz"));
    let checksum = PathBuf::from(format!("{}.sha256", source.display()));
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo did not set OUT_DIR"))
        .join("patchright-runtime.tar.gz");

    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-changed={}", checksum.display());
    if source.is_file() {
        let hash = fs::read_to_string(&checksum)
            .expect("read Patchright runtime checksum")
            .trim()
            .to_owned();
        assert!(!hash.is_empty(), "Patchright runtime checksum is empty");
        let archive = fs::read(&source).expect("read Patchright runtime");
        let actual = format!("{:x}", Sha256::digest(&archive));
        assert_eq!(actual, hash, "Patchright runtime checksum mismatch");
        fs::write(&output, archive).expect("copy embedded Patchright runtime");
        println!("cargo:rustc-env=ROPE_PATCHRIGHT_RUNTIME_HASH={hash}");
        println!("cargo:rustc-env=ROPE_PATCHRIGHT_RUNTIME_TARGET={target}");
    } else {
        fs::write(&output, []).expect("create empty Patchright runtime placeholder");
        println!("cargo:rustc-env=ROPE_PATCHRIGHT_RUNTIME_HASH=");
        println!("cargo:rustc-env=ROPE_PATCHRIGHT_RUNTIME_TARGET={target}");
        println!(
            "cargo:warning=Patchright runtime is not embedded; run scripts/prepare-patchright-runtime.sh before building to enable web tools"
        );
    }
}
