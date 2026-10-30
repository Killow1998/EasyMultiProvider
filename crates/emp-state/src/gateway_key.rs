//! The caller credential belongs to the EMP instance, independently of Codex login.

use crate::{create_private_directory, read_file_limited, write_new_private_file};
use std::{io, path::Path};

pub fn load_or_create(path: &Path) -> io::Result<String> {
    match read_file_limited(path, 80) {
        Ok(bytes) => {
            let key = String::from_utf8(bytes).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "EMP gateway key is invalid")
            })?;
            if !key.strip_prefix("emp-local-").is_some_and(|value| {
                value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            }) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "EMP gateway key is invalid",
                ));
            }
            return Ok(key);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut bytes = [0; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| io::Error::other("EMP gateway key generation failed"))?;
    let key = format!(
        "emp-local-{}",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    create_private_directory(path.parent().expect("gateway state directory"))
        .map_err(io::Error::other)?;
    write_new_private_file(path, key.as_bytes()).map_err(io::Error::other)?;
    Ok(key)
}
