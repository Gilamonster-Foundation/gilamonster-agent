//! Co-located Linux network guard for Gila's optional OCAP posture.
//!
//! Newt's confined executor discovers this helper beside `gila`, installs the
//! kernel egress-deny floor, and then executes the requested child. Other
//! platforms fail closed through the shared Newt implementation.

fn main() -> ! {
    newt_core::netguard::run_guard_and_exec(std::env::args_os().skip(1))
}
