//! Full-stack end-to-end test: load `fixtures/hello` through the real
//! hunk loader, run it on the real `M68kCpu` backend through
//! [`volamos_core::dispatch::Runtime`], and check the observable result
//! (captured stdout, exit code, and that no error occurred) rather than
//! poking at raw memory/opcodes the way `hello_fixture.rs` does.

use volamos_core::backend::{M68kCpu, TRAP_TABLE_END};
use volamos_core::dispatch::{Runtime, StartConfig};
use volamos_core::memory::FlatMemory;
use volamos_core::{TraceEvent, loader};

/// The committed fixture binary, embedded at compile time.
const HELLO: &[u8] = include_bytes!("../../../fixtures/hello");

#[test]
fn hello_fixture_runs_to_completion_with_expected_output() {
    let file = loader::parse(HELLO).expect("fixtures/hello should be a well-formed hunk file");

    let mut mem = FlatMemory::new(0x2_0000);
    let load_result =
        loader::load(&file, &mut mem, TRAP_TABLE_END).expect("fixture should load cleanly");

    let cpu = M68kCpu::new();
    let mut runtime = Runtime::new(
        cpu,
        mem,
        StartConfig {
            entry: load_result.entry,
            load_end: load_result.end,
            args: Vec::new(),
            ..StartConfig::default()
        },
    );

    let mut out = Vec::new();
    let mut events: Vec<TraceEvent> = Vec::new();
    let exit_code = runtime
        .run(
            &mut out,
            Some(&mut |ev: &TraceEvent| events.push(ev.clone())),
        )
        .expect("hello should run to a clean exit, not error out");

    assert_eq!(exit_code, 0, "hello exits with process code 0");
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "Hello from volamos\n",
        "PutStr's argument should have reached the captured output verbatim"
    );

    // Exactly one library call: PutStr.
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].library, "dos.library");
    assert_eq!(events[0].handler_name, "PutStr");
}
