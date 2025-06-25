use std::env;

fn main() {
    // Read the variable from the environment the build script is running in.
    let version = env::var("CARGO_PACKAGE_VERSION").unwrap_or_else(|_| "unknown".to_string());

    // Tell cargo to re-run the build script if this environment variable changes.
    println!("cargo:rerun-if-env-changed=CARGO_PACKAGE_VERSION");

    // Expose the version to the Rust code as an environment variable at compile time.
    // The CARGO_PKG_VERSION is the one from Cargo.toml, but we're overriding it for the build.
    // The main code will use env!("CARGO_PKG_VERSION"). Cargo automatically sets this,
    // and our env var on the build command overrides what it sees.
    println!("cargo:rustc-env=CARGO_PACKAGE_VERSION={}", version);

    // Print it as a cargo instruction, which will show up in build logs.
    println!("cargo:warning=Building package version: {}", version);
}
