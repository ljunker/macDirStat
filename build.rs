use std::{env, fs};

fn main() {
    println!("cargo:rerun-if-changed=VERSION");

    let version = fs::read_to_string("VERSION")
        .expect("VERSION must exist")
        .trim()
        .to_owned();

    assert!(
        is_release_version(&version),
        "VERSION must use the format MAJOR.MINOR.PATCH"
    );

    let cargo_version =
        env::var("CARGO_PKG_VERSION").expect("Cargo must provide CARGO_PKG_VERSION");
    assert_eq!(
        version, cargo_version,
        "VERSION and the package version in Cargo.toml must match"
    );

    println!("cargo:rustc-env=MACDIRSTAT_VERSION={version}");
}

fn is_release_version(version: &str) -> bool {
    let parts = version.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part == &"0" || !part.starts_with('0'))
        })
}
