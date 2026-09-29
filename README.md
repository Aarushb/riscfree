# asaccess

Working title for an accessibility-first RISC-V assembler and emulator: a native desktop app that does everything RARS does, built so screen reader users get the full experience instead of a stripped-down one.

## Why?

I'm a visually impaired computing science student, and RARS is the simulator my courses actually use for RISC-V. It's a solid simulator wrapped in a Java Swing UI that screen readers can barely navigate, so I built [rars_access](https://github.com/Aarushb/rars_access), a command-line wrapper that makes the common workflows usable. A wrapper over an inaccessible GUI is still a workaround, though. This project is the real thing: an IDE where accessibility is part of the architecture, not bolted on after. Sighted students get a fast, modern desktop app, and blind and low-vision students get the same app with every register, memory view, assembler error, and runtime event reachable and announced.

## Status

In development. Phase 0 (technical spikes on the GUI toolkit, screen reader speech bridge, and the emulator core) is underway; the approved product requirements and technical spec are held locally in `docs/`.

## Documentation

- [Building](docs/BUILDING.md): toolchain setup for the native GUI and speech layers.
- [Compatibility](docs/COMPAT.md): deliberate behavior differences from RARS.

## License

[MIT](LICENSE)
