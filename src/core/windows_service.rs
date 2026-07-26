use anyhow::{Context as _, bail};
use platform_lib::{
    Error as WindowsServiceError,
    service::{ServiceAccess, ServiceState},
    service_manager::{ServiceManager, ServiceManagerAccess},
};
use std::path::{Path, PathBuf};
use std::time::Duration;

const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
const ERROR_SERVICE_NOT_ACTIVE: i32 = 1062;
const ERROR_SERVICE_MARKED_FOR_DELETE: i32 = 1072;
const POLL_ATTEMPTS: usize = 200;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const OWNED_WINDOWS_STATE_DIRECTORIES: &[&str] = &[crate::SERVICE_SLUG];

fn has_raw_error(error: &WindowsServiceError, code: i32) -> bool {
    matches!(error, WindowsServiceError::Winapi(error) if error.raw_os_error() == Some(code))
}

fn wait_for_service_deletion(
    service_manager: &ServiceManager,
    service_name: &str,
) -> anyhow::Result<()> {
    for attempt in 0..POLL_ATTEMPTS {
        match service_manager.open_service(service_name, ServiceAccess::QUERY_STATUS) {
            Ok(service) => drop(service),
            Err(error) if has_raw_error(&error, ERROR_SERVICE_DOES_NOT_EXIST) => return Ok(()),
            Err(error) if has_raw_error(&error, ERROR_SERVICE_MARKED_FOR_DELETE) => {}
            Err(error) => return Err(error.into()),
        }
        if attempt + 1 < POLL_ATTEMPTS {
            std::thread::sleep(POLL_INTERVAL);
        }
    }
    bail!("timed out waiting for Windows service {service_name:?} to be deleted")
}

/// Stops and deletes a Windows service, waiting until SCM no longer exposes it.
///
/// Returns `true` when the service existed (including already being marked for
/// deletion), and `false` when it was already absent.
pub fn remove_windows_service_if_exists(service_name: &str) -> anyhow::Result<bool> {
    let service_manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    let service = match service_manager.open_service(service_name, access) {
        Ok(service) => service,
        Err(error) if has_raw_error(&error, ERROR_SERVICE_DOES_NOT_EXIST) => return Ok(false),
        Err(error) if has_raw_error(&error, ERROR_SERVICE_MARKED_FOR_DELETE) => {
            wait_for_service_deletion(&service_manager, service_name)?;
            return Ok(true);
        }
        Err(error) => return Err(error.into()),
    };

    if service.query_status()?.current_state != ServiceState::Stopped {
        if let Err(error) = service.stop()
            && !has_raw_error(&error, ERROR_SERVICE_NOT_ACTIVE)
        {
            return Err(error.into());
        }
        for attempt in 0..POLL_ATTEMPTS {
            match service.query_status() {
                Ok(status) if status.current_state == ServiceState::Stopped => break,
                Ok(_) => {}
                Err(error)
                    if has_raw_error(&error, ERROR_SERVICE_DOES_NOT_EXIST)
                        || has_raw_error(&error, ERROR_SERVICE_MARKED_FOR_DELETE) =>
                {
                    break;
                }
                Err(error) => return Err(error.into()),
            }
            if attempt + 1 == POLL_ATTEMPTS {
                bail!("timed out waiting for Windows service {service_name:?} to stop");
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    if let Err(error) = service.delete()
        && !has_raw_error(&error, ERROR_SERVICE_DOES_NOT_EXIST)
        && !has_raw_error(&error, ERROR_SERVICE_MARKED_FOR_DELETE)
    {
        return Err(error.into());
    }
    drop(service);
    wait_for_service_deletion(&service_manager, service_name)?;
    Ok(true)
}

/// Removes only this application's current service-private ProgramData tree.
///
/// The exact roots are validated immediately before deletion. Reparse-point
/// roots are rejected so an uninstall cannot be redirected outside ProgramData.
/// Legacy service state is deliberately left to the user or its owning app.
pub fn purge_windows_service_state() -> anyhow::Result<Vec<PathBuf>> {
    let program_data = windows_program_data().unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    let mut removed = Vec::new();
    for directory_name in OWNED_WINDOWS_STATE_DIRECTORIES {
        let target = program_data.join(directory_name);
        match validate_purge_target(&program_data, &target, directory_name)? {
            PurgeTarget::Missing => {}
            PurgeTarget::Directory => {
                std::fs::remove_dir_all(&target)
                    .with_context(|| format!("failed to remove service state {target:?}"))?;
                removed.push(target);
            }
        }
    }
    Ok(removed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PurgeTarget {
    Missing,
    Directory,
}

fn validate_purge_target(
    program_data: &Path,
    target: &Path,
    expected_name: &str,
) -> anyhow::Result<PurgeTarget> {
    use std::os::windows::fs::MetadataExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    if target.parent() != Some(program_data)
        || target.file_name().and_then(|value| value.to_str()) != Some(expected_name)
    {
        bail!("refusing to purge a non-service ProgramData path: {target:?}");
    }
    let metadata = match std::fs::symlink_metadata(target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PurgeTarget::Missing);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {target:?}"));
        }
    };
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        bail!("refusing to purge a non-directory or reparse-point service root: {target:?}");
    }

    let canonical_parent = std::fs::canonicalize(program_data)
        .with_context(|| format!("failed to resolve ProgramData root {program_data:?}"))?;
    let canonical_target = std::fs::canonicalize(target)
        .with_context(|| format!("failed to resolve service root {target:?}"))?;
    if canonical_target.parent() != Some(canonical_parent.as_path())
        || canonical_target
            .file_name()
            .and_then(|value| value.to_str())
            != Some(expected_name)
    {
        bail!("resolved service root escaped ProgramData: {canonical_target:?}");
    }
    Ok(PurgeTarget::Directory)
}

fn windows_program_data() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramData, SHGetKnownFolderPath};

    let mut raw = std::ptr::null_mut();
    let status =
        unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramData, 0, std::ptr::null_mut(), &mut raw) };
    if status < 0 || raw.is_null() {
        return None;
    }
    let length = unsafe {
        let mut length = 0;
        while *raw.add(length) != 0 {
            length += 1;
        }
        length
    };
    let value = std::ffi::OsString::from_wide(unsafe { std::slice::from_raw_parts(raw, length) });
    unsafe { CoTaskMemFree(raw.cast()) };
    Some(PathBuf::from(value))
}

#[cfg(test)]
mod tests {
    use super::{
        OWNED_WINDOWS_STATE_DIRECTORIES, PurgeTarget, remove_windows_service_if_exists,
        validate_purge_target,
    };

    #[test]
    fn removing_a_missing_service_is_idempotent() -> anyhow::Result<()> {
        let service_name = format!("clash_service_ipc_missing_test_{}", std::process::id());

        assert!(!remove_windows_service_if_exists(&service_name)?);
        assert!(!remove_windows_service_if_exists(&service_name)?);
        Ok(())
    }

    #[test]
    fn uninstall_owns_only_the_current_service_state_directory() {
        assert_eq!(OWNED_WINDOWS_STATE_DIRECTORIES, &[crate::SERVICE_SLUG]);
        assert!(!OWNED_WINDOWS_STATE_DIRECTORIES.contains(&crate::LEGACY_SERVICE_SLUG));
    }

    #[test]
    fn purge_validation_accepts_only_the_exact_non_reparse_directory() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!(
            "clash-service-uninstall-validation-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join(crate::SERVICE_SLUG))?;

        assert_eq!(
            validate_purge_target(&root, &root.join(crate::SERVICE_SLUG), crate::SERVICE_SLUG)?,
            PurgeTarget::Directory
        );
        assert!(
            validate_purge_target(
                &root,
                &root.join(format!("{}-escape", crate::SERVICE_SLUG)),
                crate::SERVICE_SLUG
            )
            .is_err()
        );

        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
