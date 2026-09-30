//! Stage prism.dll beside the built executable. The speech crate links it,
//! and Windows resolves it from the executable's directory first; without
//! this the app dies with STATUS_DLL_NOT_FOUND when launched outside a shell
//! that happens to have the Prism bin dir on PATH.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=PRISM_LIB_DIR");
    let Ok(lib_dir) = std::env::var("PRISM_LIB_DIR") else {
        return;
    };
    let dll = Path::new(&lib_dir)
        .parent()
        .map(|p| p.join("bin").join("prism.dll"))
        .unwrap_or_default();
    if !dll.exists() {
        // Building without the speech bridge's native library is allowed;
        // speech simply won't be usable at runtime.
        println!(
            "cargo:warning=prism.dll not found at {}; speech will be unavailable",
            dll.display()
        );
        return;
    }
    // OUT_DIR is target/<profile>/build/<pkg>-<hash>/out; three levels up is
    // target/<profile>, where the executable lands.
    let Some(out_dir) = std::env::var_os("OUT_DIR").map(std::path::PathBuf::from) else {
        return;
    };
    let Some(profile_dir) = out_dir.ancestors().nth(3) else {
        return;
    };
    let dest = profile_dir.join("prism.dll");
    if std::fs::copy(&dll, &dest).is_ok() {
        println!("cargo:rerun-if-changed={}", dll.display());
    }
}
