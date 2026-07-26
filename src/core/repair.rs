use anyhow::{Context as _, Result};
#[cfg(unix)]
use std::fs::{File, OpenOptions};

pub const REPAIR_IN_PROGRESS_EXIT_CODE: i32 = 75;

pub struct ServiceRepairGate {
    #[cfg(unix)]
    _file: File,
    #[cfg(windows)]
    mutex: windows_sys::Win32::Foundation::HANDLE,
}

pub fn acquire_service_repair_gate() -> Result<Option<ServiceRepairGate>> {
    #[cfg(unix)]
    {
        let directory = crate::prepare_service_install_directory()?;
        let path = directory.join(".repair.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("failed to open service repair gate {path:?}"))?;

        use std::os::fd::AsRawFd as _;
        use std::os::unix::fs::PermissionsExt as _;

        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to secure service repair gate {path:?}"))?;
        let status = unsafe {
            platform_lib::flock(
                file.as_raw_fd(),
                platform_lib::LOCK_EX | platform_lib::LOCK_NB,
            )
        };
        if status == 0 {
            return Ok(Some(ServiceRepairGate { _file: file }));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(error).with_context(|| format!("failed to lock service repair gate {path:?}"));
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
        use windows_sys::Win32::System::Threading::CreateMutexW;

        let name = format!(
            r"Global\clash-service-{}-repair",
            crate::CHANNEL_IDENTITY.id
        );
        let mut wide: Vec<u16> = std::ffi::OsStr::new(&name).encode_wide().collect();
        wide.push(0);
        let mutex = unsafe { CreateMutexW(std::ptr::null(), true.into(), wide.as_ptr()) };
        if mutex.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("failed to create the service repair mutex");
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(mutex) };
            return Ok(None);
        }
        Ok(Some(ServiceRepairGate { mutex }))
    }
}

#[cfg(windows)]
impl Drop for ServiceRepairGate {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::ReleaseMutex;

        unsafe {
            ReleaseMutex(self.mutex);
            CloseHandle(self.mutex);
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::acquire_service_repair_gate;
    use serial_test::serial;

    #[test]
    #[serial]
    fn windows_repair_mutex_is_exclusive_and_released_on_drop() -> anyhow::Result<()> {
        let gate = acquire_service_repair_gate()?.expect("first repair gate must be acquired");
        assert!(
            acquire_service_repair_gate()?.is_none(),
            "a concurrent repair gate must be rejected"
        );
        drop(gate);
        assert!(
            acquire_service_repair_gate()?.is_some(),
            "repair gate must be reusable after release"
        );
        Ok(())
    }
}
