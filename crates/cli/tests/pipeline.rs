//! End-to-end pipeline test: assemble a real course-style program, run it
//! against a scripted host, and check observable behavior. This is the
//! walking-skeleton contract for the whole core.

use rvasm::{AsmConfig, InputFile};
use rvm::{Event, Halt, Machine, MachineConfig, ScriptHost};

const FIB: &str = "\
# Print the first 12 Fibonacci numbers, one per line.
    li s0, 12
    li a1, 0
    li a2, 1
loop:
    beqz s0, done
    mv a0, a1
    li a7, 1
    ecall
    li a7, 11
    li a0, 10
    ecall
    mv a3, a1
    mv a1, a2
    add a2, a3, a2
    addi s0, s0, -1
    j loop
done:
    li a7, 10
    ecall
";

#[test]
fn fibonacci_pipeline() {
    let files = vec![InputFile {
        name: "fib.s".into(),
        source: FIB.into(),
    }];
    let result = rvasm::assemble(&files, &AsmConfig::default());
    assert!(!result.has_errors(), "diags: {:?}", result.diagnostics);
    let program = result.program.unwrap();
    assert_eq!(program.statements.len(), 17);

    let host = ScriptHost::default();
    let mut machine = Machine::new(program, Box::new(host.clone()), MachineConfig::default());
    let events = machine.run(None);

    let expected: Vec<i32> = (0..12)
        .scan((0i32, 1i32), |st, _| {
            let out = st.0;
            *st = (st.1, st.0 + st.1);
            Some(out)
        })
        .collect();
    let printed = expected
        .iter()
        .map(|n| format!("{n}\n"))
        .collect::<String>();
    assert_eq!(host.take_output(), printed);
    assert_eq!(events.last(), Some(&Event::Halted(Halt::Exit { code: 0 })));
    // Prologue: 3 (each `li` is one addi here) + 12 iterations x 12 loop
    // instructions + 2 exit instructions.
    assert_eq!(machine.instret(), 3 + 12 * 12 + 2);
}

#[test]
fn diagnostics_surface_file_line_col() {
    let files = vec![
        InputFile {
            name: "a.s".into(),
            source: "j missing\n".into(),
        },
        InputFile {
            name: "b.s".into(),
            source: "bogus a0, a1\n".into(),
        },
    ];
    let result = rvasm::assemble(&files, &AsmConfig::default());
    assert!(result.has_errors());
    assert_eq!(result.diagnostics.len(), 2);
    // Pass-one diagnostics (unknown mnemonic) come before pass-two
    // (unresolved symbol) diagnostics, so look them up by code.
    let undef = result
        .diagnostics
        .iter()
        .find(|d| d.code == "E-UNDEF")
        .expect("E-UNDEF diagnostic");
    assert_eq!(files[undef.pos.file].name, "a.s");
    assert_eq!(undef.pos.line, 1);
    let mnemonic = result
        .diagnostics
        .iter()
        .find(|d| d.code == "E-MNEMONIC")
        .expect("E-MNEMONIC diagnostic");
    assert_eq!(files[mnemonic.pos.file].name, "b.s");
    assert_eq!(mnemonic.pos.line, 1);
}
