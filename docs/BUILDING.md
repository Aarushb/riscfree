# Building AsAccess

The workspace has two kinds of crates. The core (`rvasm`, `rvm`, `narration`, `asaccess-cli`) is pure Rust: `cargo test` just works. The GUI (`asaccess`) and speech (`speech`) layers wrap native libraries and need the toolchain below on Windows. Everything is project-scoped; nothing installs machine-wide.

## Prerequisites

- Rust (MSVC toolchain) and Visual Studio Build Tools with the C++ workload. The bundled CMake and Ninja are used from the Build Tools directory, so the standalone CMake install is not needed.
- [uv](https://docs.astral.sh/uv/) for the Python-side tooling (the UIA test probe and the libclang wheel).
- Python 3.12+ (uv can fetch it).

## One-time setup

1. Create the tooling venv and put libclang in it (bindgen needs the DLL; there is no pregenerated-bindings path in wxdragon-sys):

   ```bash
   uv venv tests/uiauto/.venv
   uv pip install --python tests/uiauto/.venv/Scripts/python.exe libclang pywinauto comtypes
   ```

2. Build Prism (the screen reader bridge) with the backends that do not need the ATL SDK, and install it inside `target/`:

   ```bash
   cmake -S "$CARGO_HOME/registry/src/<registry>/prism-sys-<ver>/prism" \
         -B target/prism-build \
         -DPRISM_ENABLE_SAPI_BACKEND=OFF -DPRISM_ENABLE_JAWS_BACKEND=OFF \
         -DPRISM_ENABLE_ZOOM_TEXT_BACKEND=OFF -DPRISM_ENABLE_SENSE_READER_BACKEND=OFF \
         -DPRISM_ENABLE_WINDOW_EYES_BACKEND=OFF \
         -DCMAKE_INSTALL_PREFIX="$(pwd)/target/prism-install"
   cmake --build target/prism-build --config Release --target install --parallel
   ```

   The off backends are exactly the ones that include `atlbase.h`. The remaining set (NVDA, OneCore, UIA, PCTalker, ZDSR, BoyPCReader) covers NVDA and Windows system voices, which is the shipping target for Windows; JAWS and SAPI come back if the VS ATL component gets installed.

3. Per shell session, before building the GUI or speech crates:

   ```bash
   export PRISM_LIB_DIR="$(pwd)/target/prism-install/lib"
   export PATH="$(pwd)/target/prism-install/bin:$PATH"                # prism.dll at runtime
   export PATH="/c/Program Files (x86)/Microsoft Visual Studio/18/BuildTools/VC/Tools/MSVC/<ver>/bin/Hostx64/x64:$PATH"
   export PATH="/c/Program Files (x86)/Microsoft Visual Studio/18/BuildTools/Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin:$PATH"
   export LIBCLANG_PATH="$(pwd)/tests/uiauto/.venv/Lib/site-packages/clang/native"
   ```

A `scripts/dev-env.ps1` may automate step 3 later; for now this file is the source of truth.

## Verifying

- `cargo test` covers the core crates with no special environment.
- `cargo run -p speech --features prism --example probe` (with the step 3 environment) speaks a test phrase through the active screen reader and prints the backends Prism found.
- `cargo run -p asaccess` opens the GUI skeleton; NVDA and JAWS should read every control.
