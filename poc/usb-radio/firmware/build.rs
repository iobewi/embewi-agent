fn main() {
    // Wi-Fi credentials are compile-time POC inputs. Make Cargo rebuild when they change
    // without ever recording their values in the repository.
    println!("cargo:rerun-if-env-changed=WIFI_SSID");
    println!("cargo:rerun-if-env-changed=WIFI_PASSWORD");

    // esp-hal's linker scripts must come last; without this the link fails with
    // undefined PAC symbols (a `cargo check` never notices).
    println!("cargo:rustc-link-arg=-Tlinkall.x");
}
