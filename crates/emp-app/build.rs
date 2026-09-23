use std::env;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=EMP_PACKAGE_RESOURCE");
    println!("cargo:rerun-if-env-changed=EMP_REQUIRE_WINDOWS_RESOURCES");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let required = env::var("EMP_REQUIRE_WINDOWS_RESOURCES")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let Some(resource_file) = env::var_os("EMP_PACKAGE_RESOURCE").map(PathBuf::from) else {
        if required {
            panic!("EMP packaging requires a compiled Windows resource file");
        }
        return;
    };

    if let Err(error) = validate_windows_resources(&resource_file) {
        if required {
            panic!("failed to embed Windows icon/version resources: {error}");
        }
        println!("cargo:warning=skipping Windows resources: {error}");
    }
}

fn validate_windows_resources(resource_file: &Path) -> Result<(), String> {
    if !resource_file.is_file() {
        return Err(format!(
            "compiled resource file does not exist: {}",
            resource_file.display()
        ));
    }
    let metadata = std::fs::metadata(resource_file)
        .map_err(|error| format!("could not inspect {}: {error}", resource_file.display()))?;
    if metadata.len() == 0 {
        return Err("resource compiler created an empty .res file".to_owned());
    }

    println!("cargo:rerun-if-changed={}", resource_file.display());
    println!("cargo:rustc-link-arg-bin=EMP={}", resource_file.display());
    Ok(())
}
