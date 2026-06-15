pub fn child_rss_bytes(child: &std::process::Child) -> Option<u64> {
    let pid = child.id();

    #[cfg(target_os = "linux")]
    {
        // /proc/PID/statm: field 1 is RSS in pages
        let statm = std::fs::read_to_string(format!("/proc/{}/statm", pid)).ok()?;
        let rss_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        Some(rss_pages * page_size)
    }

    #[cfg(target_os = "macos")]
    {
        // proc_pidinfo with PROC_PIDTASKINFO
        use std::mem;

        extern "C" {
            fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut u8, buffersize: i32) -> i32;
        }

        #[repr(C)]
        struct proc_taskinfo {
            pti_virtual_size:      u64,
            pti_resident_size:     u64,
            pti_total_user:        u64,
            pti_total_system:      u64,
            pti_threads_user:      u64,
            pti_threads_system:    u64,
            pti_policy:            i32,
            pti_faults:            i32,
            pti_pageins:           i32,
            pti_cow_faults:        i32,
            pti_messages_sent:     i32,
            pti_messages_received: i32,
            pti_syscalls_mach:     i32,
            pti_syscalls_unix:     i32,
            pti_csw:               i32,
            pti_threadnum:         i32,
            pti_numrunning:        i32,
            pti_priority:          i32,
        }

        const PROC_PIDTASKINFO: i32 = 4;

        let mut info: proc_taskinfo = unsafe { mem::zeroed() };
        let ret = unsafe {
            proc_pidinfo(
                pid as i32,
                PROC_PIDTASKINFO,
                0,
                &mut info as *mut _ as *mut u8,
                mem::size_of::<proc_taskinfo>() as i32,
            )
        };

        if ret <= 0 { return None }
        Some(info.pti_resident_size)
    }

    #[cfg(target_os = "windows")]
    {
        use std::mem;

        #[link(name = "psapi")]
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut u8;
            fn CloseHandle(handle: *mut u8) -> i32;
            fn GetProcessMemoryInfo(process: *mut u8, counters: *mut PROCESS_MEMORY_COUNTERS, size: u32) -> i32;
        }

        #[repr(C)]
        struct PROCESS_MEMORY_COUNTERS {
            cb:                          u32,
            page_fault_count:            u32,
            peak_working_set_size:       usize,
            working_set_size:            usize,  // this is RSS
            quota_peak_paged_pool_usage: usize,
            quota_paged_pool_usage:      usize,
            quota_peak_np_pool_usage:    usize,
            quota_np_pool_usage:         usize,
            pagefile_usage:              usize,
            peak_pagefile_usage:         usize,
        }

        const PROCESS_QUERY_INFORMATION: u32 = 0x0400;
        const PROCESS_VM_READ: u32 = 0x0010;

        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
            if handle.is_null() { return None; }
            let mut counters: PROCESS_MEMORY_COUNTERS = mem::zeroed();
            counters.cb = mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            let ok = GetProcessMemoryInfo(handle, &mut counters, counters.cb);
            CloseHandle(handle);
            if ok == 0 { return None; }
            Some(counters.working_set_size as u64)
        }
    }
}
