use std::env;

fn main() {
    // Read the variable from the environment the build script is running in.
    let version = env::var("CARGO_PACKAGE_VERSION").unwrap_or_else(|_| "unknown".to_string());

    // Print it as a cargo instruction, which will show up in build logs.
    println!("cargo:warning=Building package version: {}", version);
}
