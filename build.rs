fn main() {
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    if std::env::var_os("CARGO_FEATURE_DESKTOP").is_some() {
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../share/hype-flow/lib");
        if let Ok(output) = std::process::Command::new("pkg-config")
            .args(["--variable=libdir", "gtk4-layer-shell-0"])
            .output()
            && output.status.success()
        {
            let directory = String::from_utf8_lossy(&output.stdout);
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", directory.trim());
        }
    }
}
