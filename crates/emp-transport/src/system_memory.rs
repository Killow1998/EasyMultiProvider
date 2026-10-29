//! Small, dependency-light system memory probes used for request admission.

use crate::MemoryStatus;

#[cfg(target_os = "linux")]
mod cgroup;

#[cfg(target_os = "linux")]
pub fn system_memory_status() -> Option<MemoryStatus> {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|contents| parse_linux_meminfo(&contents))
        .map(cgroup::constrain)
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
    // SAFETY: this statistics structure contains only integer counters.
    let mut statistics: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: the host API writes `count` integer words into the correctly sized
    // vm_statistics64 buffer, and the host/flavor/count match Apple's ABI.
    #[allow(deprecated)] // libc deprecates mach_host_self; its system ABI remains required here.
    let result = unsafe {
        libc::host_statistics64(
            libc::mach_host_self(),
            libc::HOST_VM_INFO64,
            (&mut statistics as *mut libc::vm_statistics64).cast(),
            &mut count,
        )
    };
    // SAFETY: vm_page_size is an OS-provided scalar initialized by libSystem.
    let page_size = unsafe { libc::vm_page_size } as u64;
    mac_status_from_statistics(total, &statistics, result, count, page_size)
}

#[cfg(target_os = "macos")]
fn mac_status_from_statistics(
    total: u64,
    statistics: &libc::vm_statistics64,
    result: libc::kern_return_t,
    count: libc::mach_msg_type_number_t,
    page_size: u64,
) -> Option<MemoryStatus> {
    // Older kernels return a shorter revision. Only require the fields we read.
    const REQUIRED_WORDS: usize = (std::mem::offset_of!(libc::vm_statistics64, inactive_count)
        + std::mem::size_of::<libc::natural_t>())
    .div_ceil(std::mem::size_of::<libc::integer_t>());
    if result != libc::KERN_SUCCESS || (count as usize) < REQUIRED_WORDS {
        return None;
    }
    // libc models Apple's packed structure. Read fields unaligned to avoid
    // creating references to packed members.
    let free_pages = unsafe { std::ptr::addr_of!(statistics.free_count).read_unaligned() } as u64;
    let inactive_pages =
        unsafe { std::ptr::addr_of!(statistics.inactive_count).read_unaligned() } as u64;
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

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_revision_prefix_preserves_memory_admission() {
        use crate::{RequestCapacityReason, RequestLimits, RequestLimitsConfig, TransportKind};
        // SAFETY: vm_statistics64 contains only integer counters.
        let mut statistics: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        statistics.free_count = 512 * 1024;
        statistics.inactive_count = 1024 * 1024;
        let total = 16 * 1024 * 1024 * 1024;
        for count in [3, 38, libc::HOST_VM_INFO64_COUNT] {
            let memory =
                mac_status_from_statistics(total, &statistics, libc::KERN_SUCCESS, count, 4096)
                    .unwrap();
            assert_eq!(memory.available, 6 * 1024 * 1024 * 1024);
            let limits = RequestLimits::new(
                RequestLimitsConfig::default(),
                move || Some(memory),
                || 0,
                "mac-revision",
            )
            .unwrap();
            let mut request = limits.request(TransportKind::Http);
            request
                .ensure(137_177_159)
                .expect("issue #66 request fits measured memory");
            assert_eq!(request.limit(), 144 * 1024 * 1024);
            assert_eq!(
                limits.snapshot().unwrap().notices[0].required_memory_bytes,
                1664 * 1024 * 1024
            );
            drop(request);
            assert_eq!(limits.snapshot().unwrap().reserved_bytes, 0);
        }
        for (result, count) in [
            (libc::KERN_SUCCESS, 0),
            (libc::KERN_SUCCESS, 2),
            (libc::KERN_FAILURE, 38),
        ] {
            assert!(mac_status_from_statistics(total, &statistics, result, count, 4096).is_none());
        }
        statistics.free_count = 0;
        statistics.inactive_count = 0;
        let memory =
            mac_status_from_statistics(total, &statistics, libc::KERN_SUCCESS, 38, 4096).unwrap();
        let limits = RequestLimits::new(
            RequestLimitsConfig::default(),
            move || Some(memory),
            || 0,
            "mac-pressure",
        )
        .unwrap();
        let error = limits
            .request(TransportKind::Http)
            .ensure(137_177_159)
            .unwrap_err();
        assert_eq!(error.reason, RequestCapacityReason::MemoryLimit);
        assert_eq!(error.available_bytes, Some(0));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_native_memory_probe_returns_a_valid_measurement() {
        let memory = system_memory_status().expect("macOS kernel statistics must be readable");
        assert_eq!(
            memory.total,
            Some(mac_total_physical_bytes().unwrap() as usize)
        );
        assert!(memory.available <= memory.total.unwrap());
        eprintln!("native macOS memory: {memory:?}");
    }

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
