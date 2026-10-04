fn main() {
    // esp-hal's linker scripts must come last; without this the link fails with
    // undefined PAC symbols (a `cargo check` never notices).
    println!("cargo:rustc-link-arg=-Tlinkall.x");
}
