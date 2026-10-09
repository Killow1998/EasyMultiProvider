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
    // Reserve our own file once. Each network retry truncates this handle,
    // resetting both byte count and digest without trusting a partial download.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| manager.io_error("write_package", error))?;
    manager.retry_network(|| {
        file.set_len(0)
            .and_then(|()| std::io::Seek::rewind(&mut file))
            .map_err(|error| manager.io_error("write_package", error))?;
        download_attempt(manager, asset, &mut file, &mut progress)
    })
}

fn download_attempt(
    manager: &UpdateManager,
    asset: &Asset,
    file: &mut std::fs::File,
    progress: &mut impl FnMut(u8),
) -> Result<()> {
    manager.stage("download_package");
    let started = Instant::now();
    let mut response = manager.open_package(&asset.url)?;
    if !response.status().is_success() {
        manager.http_error(response.status().as_u16());
        return Err(UpdateError("update_failed"));
    }
    let mut digest = Sha256::new();
    let mut size = 0_u64;
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut buffer = [0_u8; 256 * 1024];
    loop {
        let read = response.read(&mut buffer).map_err(|error| {
            manager.trace_download(size, asset.size, started.elapsed(), "interrupted");
            manager.io_error("download_package", error);
            UpdateError("update_failed")
        })?;
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
        file.write_all(&buffer[..read])
            .map_err(|error| manager.io_error("write_package", error))?;
        progress((size.saturating_mul(100) / asset.size).min(99) as u8);
    }
    file.sync_all()
        .map_err(|error| manager.io_error("sync_package", error))?;
    manager.trace_download(size, asset.size, started.elapsed(), "received");
    if size < asset.size {
        // A complete HTTP response can still have a truncated package body.
        manager.incomplete_download();
        return Err(UpdateError("update_failed"));
    }
    manager.stage("verify_checksum");
    if size != asset.size || format!("{:x}", digest.finalize()) != asset.digest {
        return Err(UpdateError("checksum_mismatch"));
    }
    Ok(())
}
