//! Release signing tool.
//!
//!   kimrashi-sign keygen [key file]   make a key pair; the private key goes to the file
//!                                     (default %USERPROFILE%\.kimrashi\signing.key), the
//!                                     public key is printed for src/sig.rs
//!   kimrashi-sign sign <exe> [key file]   write <exe>.sig
//!
//! The version signed is this tool's own, so build both from the same checkout.

#[path = "../src/sig.rs"]
mod sig;

use ed25519_dalek::{Signer, SigningKey};
use rand_core::RngCore;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

fn default_key() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_default();
    home.join(".kimrashi").join("signing.key")
}

fn load(path: &PathBuf) -> Result<SigningKey, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(SigningKey::from_bytes(&sig::unhex::<32>(&text).ok_or("the key file is not 64 hex characters")?))
}

fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("keygen") => {
            let path = args.get(1).map(PathBuf::from).unwrap_or_else(default_key);
            if path.exists() {
                return Err(format!("{} already exists; refusing to overwrite a signing key", path.display()));
            }
            let mut seed = [0u8; 32];
            rand_core::OsRng.try_fill_bytes(&mut seed).map_err(|e| e.to_string())?;
            std::fs::create_dir_all(path.parent().ok_or("bad path")?).map_err(|e| e.to_string())?;
            std::fs::write(&path, format!("{}\n", sig::hex(&seed))).map_err(|e| e.to_string())?;
            let public = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
            println!("private key written to {} (back it up; keep it out of the repository)", path.display());
            println!("public key for src/sig.rs:");
            println!("pub const PUBLIC_KEY: [u8; 32] = {public:?};");
            Ok(())
        }
        Some("sign") => {
            let exe = PathBuf::from(args.get(1).ok_or("usage: kimrashi-sign sign <exe> [key file]")?);
            let key = load(&args.get(2).map(PathBuf::from).unwrap_or_else(default_key))?;
            if key.verifying_key().to_bytes() != sig::PUBLIC_KEY {
                return Err("this key is not the public key built into src/sig.rs; the updater would reject it".into());
            }
            let hash: [u8; 32] = Sha256::digest(std::fs::read(&exe).map_err(|e| format!("cannot read {}: {e}", exe.display()))?).into();
            let version = {
                let parts: Vec<u64> = env!("CARGO_PKG_VERSION").split('.').map(|p| p.parse().unwrap()).collect();
                [parts[0], parts[1], parts[2]]
            };
            let signature = key.sign(&sig::message(version, &hash));
            let out = PathBuf::from(format!("{}.sig", exe.display()));
            std::fs::write(&out, format!("{}\n", sig::hex(&signature.to_bytes()))).map_err(|e| e.to_string())?;
            sig::verify(&sig::PUBLIC_KEY, version, &hash, &std::fs::read_to_string(&out).unwrap())?;
            println!("signed {} as {}, version {}, sha256 {}", exe.display(), out.display(), env!("CARGO_PKG_VERSION"), sig::hex(&hash));
            Ok(())
        }
        _ => Err("usage: kimrashi-sign keygen [key file] | sign <exe> [key file]".into()),
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
