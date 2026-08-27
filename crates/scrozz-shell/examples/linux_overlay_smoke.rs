//! Entry point for the native Linux overlay smoke probes.

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("SKIP: linux_overlay_smoke requires a Linux host");
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    scrozz_shell::linux::smoke::run()
}
