# Building RISC-Free

The workspace has two kinds of crates. The core (`rvasm`, `rvm`, `narration`, `riscfree-cli`) is pure Rust: `cargo test` just works. The GUI (`riscfree`) and speech (`speech`) layers wrap native libraries and need the toolchain below on Windows. Everything is project-scoped; nothing installs machine-wide beyond the standard VS Build Tools workload.

## Prerequisites

- Rust (MSVC toolchain) and Visual Studio Build Tools with the C++ workload plus the C++ ATL component (Prism's JAWS/SAPI/ZoomText backends include `atlbase.h`).
- [uv](https://docs.astral.sh/uv/) for the Python-side tooling (the UIA test probe and the libclang wheel).
- Python 3.12+ (uv can fetch it).
- CMake and Ninja: the copies bundled with Build Tools work (see the PATH entries below); no standalone install needed.

## One-time setup

1. Create the tooling venv and put libclang in it (bindgen needs the DLL; there is no pregenerated-bindings path in wxdragon-sys):

   ```bash
   uv venv tests/uiauto/.venv
   uv pip install --python tests/uiauto/.venv/Scripts/python.exe libclang pywinauto comtypes
   ```

2. Build Prism (the screen reader bridge) and install it inside `target/`:

   ```bash
   cmake -S "$CARGO_HOME/registry/src/<registry>/prism-sys-<ver>/prism" \
         -B target/prism-build \
         -DCMAKE_INSTALL_PREFIX="$(pwd)/target/prism-install"
   cmake --build target/prism-build --config Release --target install --parallel
   ```

   This builds every backend the platform supports (NVDA, JAWS, SAPI, OneCore, UIA and friends).

3. Per shell session, before building the GUI or speech crates, two VS Build Tools folders must be on PATH: MSVC `link.exe`, and the standalone CMake **Ninja** folder (`...\Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja`), whose `ninja.exe` both debug and release builds of the wxDragon C++ glue need. `LIBCLANG_PATH`, `PRISM_LIB_DIR`, and `CMAKE` are set workspace-wide by `.cargo/config.toml`:

   ```bash
   export PATH="/c/Program Files (x86)/Microsoft Visual Studio/18/BuildTools/VC/Tools/MSVC/<ver>/bin/Hostx64/x64:$PATH"
   export PATH="/c/Program Files (x86)/Microsoft Visual Studio/18/BuildTools/Common7/IDE/CommonExtensions/Microsoft/CMake/Ninja:$PATH"
   export LIBCLANG_PATH="$(pwd)/tests/uiauto/.venv/Lib/site-packages/clang/native"
   ```

A `scripts/dev-env.ps1` may automate this later; for now this file is the source of truth.

## Packaging (portable zip)

The distribution is a plain zip: no installer UI to verify with a screen reader, no admin rights, updates by replacing the files.

```bash
cargo build --release -p riscfree -p riscfree-cli
python scripts/package.py
```

The zip lands in `target/dist/riscfree-<version>-windows-x64.zip` with the GUI exe, the headless CLI, `prism.dll` (staged beside the binaries by the build), and a `FIRST-RUN.txt` pointing at F1 for shortcuts and the File > Reconnect speech fallback.

## Verifying

- `cargo test` covers the core crates with no special environment.
- `cargo run -p speech --features prism --example probe` (with the step 3 environment) speaks a test phrase through the active screen reader and prints the backends Prism found.
- `cargo run -p riscfree` opens the GUI; NVDA and JAWS should read every control.
