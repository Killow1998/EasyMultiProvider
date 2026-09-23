//! Bounded package streaming and digest verification.
use super::manager::UpdateManager;
use super::release::{Asset, MAX_PACKAGE_BYTES};
use super::{Result, UpdateError};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

pub(super) fn download_package<F>(
    manager: &UpdateManager,
    asset: &Asset,
    destination: &Path,
    mut progress: F,
) -> Result<()>
where
    F: FnMut(u8),
{
    if asset.size == 0 || asset.size > MAX_PACKAGE_BYTES {
        return Err(UpdateError("invalid_package_size"));
    }
    let mut response = manager.open_package(&asset.url)?;
    if !response.status().is_success() {
        return Err(UpdateError("update_failed"));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let mut digest = Sha256::new();
    let mut size = 0_u64;
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut buffer = [0_u8; 256 * 1024];
    loop {
        let read = response
            .read(&mut buffer)
            .map_err(|_| UpdateError("update_failed"))?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .ok_or(UpdateError("invalid_package_size"))?;
        if size > asset.size || size > MAX_PACKAGE_BYTES || Instant::now() > deadline {
            return Err(UpdateError("invalid_package_size"));
        }
        digest.update(&buffer[..read]);
        file.write_all(&buffer[..read])?;
        progress((size.saturating_mul(100) / asset.size).min(99) as u8);
    }
    file.sync_all()?;
    if size != asset.size || format!("{:x}", digest.finalize()) != asset.digest {
        return Err(UpdateError("checksum_mismatch"));
    }
    Ok(())
}
