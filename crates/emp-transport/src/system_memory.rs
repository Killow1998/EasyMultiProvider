//! Small, dependency-light system memory probes used for request admission.

use crate::MemoryStatus;

#[cfg(target_os = "linux")]
pub fn system_memory_status() -> Option<MemoryStatus> {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|contents| parse_linux_meminfo(&contents))
}

#[cfg(windows)]
pub fn system_memory_status() -> Option<MemoryStatus> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: GlobalMemoryStatusEx writes a documented fixed-size structure.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return None;
    }
    status_from_bytes(status.ullTotalPhys, status.ullAvailPhys)
}

#[cfg(target_os = "macos")]
pub fn system_memory_status() -> Option<MemoryStatus> {
    let total = mac_total_physical_bytes()?;
    let mut statistics = std::mem::MaybeUninit::<libc::vm_statistics64>::zeroed();
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: the host API writes `count` integer words into the correctly sized
    // vm_statistics64 buffer, and the host/flavor/count match Apple's ABI.
    #[allow(deprecated)] // libc deprecates mach_host_self; its system ABI remains required here.
    let result = unsafe {
        libc::host_statistics64(
            libc::mach_host_self(),
            libc::HOST_VM_INFO64,
            statistics.as_mut_ptr().cast(),
            &mut count,
        )
    };
    if result != libc::KERN_SUCCESS || count < libc::HOST_VM_INFO64_COUNT {
        return None;
    }
    // SAFETY: host_statistics64 succeeded and returned the complete structure.
    let statistics = unsafe { statistics.assume_init() };
    // libc models Apple's packed structure. Read fields unaligned to avoid
    // creating references to packed members.
    let free_pages = unsafe { std::ptr::addr_of!(statistics.free_count).read_unaligned() } as u64;
    let inactive_pages =
        unsafe { std::ptr::addr_of!(statistics.inactive_count).read_unaligned() } as u64;
    // SAFETY: vm_page_size is an OS-provided scalar initialized by libSystem.
    let page_size = unsafe { libc::vm_page_size } as u64;
    let available = bytes_from_pages(free_pages, inactive_pages, page_size)?;
    status_from_bytes(total, available)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn system_memory_status() -> Option<MemoryStatus> {
    None
}

#[cfg(target_os = "macos")]
fn mac_total_physical_bytes() -> Option<u64> {
    let mut total = 0_u64;
    let mut length = std::mem::size_of::<u64>();
    // SAFETY: sysctlbyname writes a u64 to the supplied buffer for hw.memsize.
    let result = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut total as *mut u64).cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    (result == 0 && length == std::mem::size_of::<u64>() && total > 0).then_some(total)
}

fn status_from_bytes(total: u64, available: u64) -> Option<MemoryStatus> {
    if total == 0 || available > total {
        return None;
    }
    let total = usize::try_from(total).ok()?;
    let available = usize::try_from(available).ok()?;
    let used = total - available;
    Some(MemoryStatus {
        available,
        total: Some(total),
        used: Some(used),
        used_percent: Some((used as f64 / total as f64) * 100.0),
    })
}

#[cfg(any(target_os = "macos", test))]
fn bytes_from_pages(free: u64, inactive: u64, page_size: u64) -> Option<u64> {
    if page_size == 0 {
        return None;
    }
    free.checked_add(inactive)?.checked_mul(page_size)
}

#[cfg(target_os = "linux")]
fn parse_linux_meminfo(contents: &str) -> Option<MemoryStatus> {
    let mut total = None;
    let mut available = None;
    for line in contents.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let target = match name {
            "MemTotal" => &mut total,
            "MemAvailable" => &mut available,
            _ => continue,
        };
        if target.is_some() {
            return None;
        }
        let mut parts = value.split_ascii_whitespace();
        let kibibytes = parts.next()?.parse::<u64>().ok()?;
        if parts.next()? != "kB" || parts.next().is_some() {
            return None;
        }
        *target = Some(kibibytes.checked_mul(1024)?);
    }
    status_from_bytes(total?, available?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_linux_memtotal_and_memavailable_in_kibibytes() {
        let status = parse_linux_meminfo(
            "MemTotal:       1000 kB\nMemFree:          10 kB\nMemAvailable:    250 kB\n",
        )
        .expect("valid Linux memory status");
        assert_eq!(status.total, Some(1_024_000));
        assert_eq!(status.available, 256_000);
        assert_eq!(status.used, Some(768_000));
        assert_eq!(status.used_percent, Some(75.0));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rejects_incomplete_malformed_overflow_and_inconsistent_meminfo() {
        for contents in [
            "MemTotal: 1000 kB\n",
            "MemTotal: one kB\nMemAvailable: 250 kB\n",
            "MemTotal: 1000 MB\nMemAvailable: 250 kB\n",
            "MemTotal: 18446744073709551615 kB\nMemAvailable: 250 kB\n",
            "MemTotal: 1000 kB\nMemAvailable: 1001 kB\n",
        ] {
            assert!(parse_linux_meminfo(contents).is_none(), "{contents}");
        }
    }

    #[test]
    fn converts_total_and_available_bytes_without_overflow_or_overcommit() {
        let status = status_from_bytes(100, 25).expect("valid memory values");
        assert_eq!(status.total, Some(100));
        assert_eq!(status.available, 25);
        assert_eq!(status.used, Some(75));
        assert_eq!(status.used_percent, Some(75.0));
        assert!(status_from_bytes(0, 0).is_none());
        assert!(status_from_bytes(100, 101).is_none());
    }

    #[test]
    fn checked_page_conversion_rejects_overflow_and_available_over_total() {
        assert_eq!(bytes_from_pages(2, 3, 4096), Some(20_480));
        assert!(bytes_from_pages(u64::MAX, 1, 4096).is_none());
        assert!(bytes_from_pages(1, 1, u64::MAX).is_none());
        assert!(bytes_from_pages(1, 1, 0).is_none());
        let available = bytes_from_pages(1, 2, 4096).unwrap();
        assert!(status_from_bytes(available - 1, available).is_none());
    }
}
