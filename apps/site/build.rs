use std::env;

fn main() {
    println!("cargo:rerun-if-env-changed=BUILD_COMMIT");
    let commit = match env::var("BUILD_COMMIT") {
        Ok(commit) if !commit.is_empty() => commit,
        _ => "unknown".to_owned(),
    };
    println!(
        "cargo:rustc-env=JOURNEY_BINARY_VERSION={} (git: {commit})",
        env!("CARGO_PKG_VERSION")
    );
}
