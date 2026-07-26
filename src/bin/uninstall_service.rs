#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn main() {
    panic!("This program is not intended to run on this platform.");
}

use anyhow::Error;

fn enter_repair_gate() -> Result<clash_service_ipc::ServiceRepairGate, Error> {
    match clash_service_ipc::acquire_service_repair_gate()? {
        Some(gate) => Ok(gate),
        None => {
            eprintln!("Service repair is already in progress");
            std::process::exit(clash_service_ipc::REPAIR_IN_PROGRESS_EXIT_CODE);
        }
    }
}

fn run_maintenance_if_requested() -> Result<bool, Error> {
    if !std::env::args().any(|argument| argument == "--cleanup-stale-owners") {
        return Ok(false);
    }
    let removed = clash_service_ipc::cleanup_stale_owner_state()?;
    println!("Removed {} stale owner state directories", removed.len());
    Ok(true)
}

#[cfg(test)]
fn poll_until<T>(
    max_attempts: usize,
    mut probe: impl FnMut() -> Result<Option<T>, Error>,
    mut pause: impl FnMut(),
    timeout_message: &str,
) -> Result<T, Error> {
    for attempt in 0..max_attempts {
        if let Some(value) = probe()? {
            return Ok(value);
        }
        if attempt + 1 < max_attempts {
            pause();
        }
    }
    Err(anyhow::anyhow!("{timeout_message}"))
}

#[cfg(target_os = "macos")]
fn main() -> Result<(), Error> {
    use std::env;
    use std::path::Path;

    if run_maintenance_if_requested()? {
        return Ok(());
    }
    let _gate = enter_repair_gate()?;
    let debug = env::args().any(|arg| arg == "--debug");

    #[cfg(not(feature = "development-channel"))]
    let _ = uninstall_old_service();
    // 定义路径
    let bundle_path = format!(
        "/Library/PrivilegedHelperTools/{}.bundle",
        clash_service_ipc::MACOS_SERVICE_ID
    );
    let plist_file = format!(
        "/Library/LaunchDaemons/{}.plist",
        clash_service_ipc::MACOS_SERVICE_ID
    );
    let service_id = clash_service_ipc::MACOS_SERVICE_ID;

    // 停止并卸载服务
    let _ = run_command("launchctl", &["stop", service_id], debug);
    let _ = run_command(
        "launchctl",
        &["disable", &format!("system/{}", service_id)],
        debug,
    );
    let _ = run_command("launchctl", &["bootout", "system", &plist_file], debug);

    // 删除文件
    if Path::new(&plist_file).exists() {
        std::fs::remove_file(&plist_file)
            .map_err(|e| anyhow::anyhow!("Failed to remove plist file: {}", e))?;
    }

    // 删除整个 bundle 目录
    if Path::new(&bundle_path).exists() {
        std::fs::remove_dir_all(&bundle_path)
            .map_err(|e| anyhow::anyhow!("Failed to remove bundle directory: {}", e))?;
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Error> {
    use std::env;

    if run_maintenance_if_requested()? {
        return Ok(());
    }
    let _gate = enter_repair_gate()?;
    let debug = env::args().any(|arg| arg == "--debug");
    let service_name = clash_service_ipc::SERVICE_SLUG;

    // Stop and disable service
    let _ = run_command(
        "systemctl",
        &["stop", &format!("{}.service", service_name)],
        debug,
    );
    let _ = run_command(
        "systemctl",
        &["disable", &format!("{}.service", service_name)],
        debug,
    );

    // Remove service file
    let unit_file = format!("/etc/systemd/system/{}.service", service_name);
    if std::path::Path::new(&unit_file).exists() {
        std::fs::remove_file(&unit_file)
            .map_err(|e| anyhow::anyhow!("Failed to remove service file: {}", e))?;
    }

    // Reload systemd
    let _ = run_command("systemctl", &["daemon-reload"], debug);
    let target = clash_service_ipc::prepare_service_install_directory()?.join("clash-service");
    if target.exists() {
        std::fs::remove_file(&target).map_err(|error| {
            anyhow::anyhow!("Failed to remove service binary {target:?}: {error}")
        })?;
    }

    Ok(())
}

/// stop and uninstall the service
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    if run_maintenance_if_requested()? {
        return Ok(());
    }
    let _gate = enter_repair_gate()?;
    clash_service_ipc::remove_windows_service_if_exists(clash_service_ipc::WINDOWS_SERVICE_NAME)?;
    let removed = clash_service_ipc::purge_windows_service_state()?;
    println!(
        "Service uninstalled successfully; removed {} private state director{}.",
        removed.len(),
        if removed.len() == 1 { "y" } else { "ies" }
    );
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn uninstall_old_service() -> Result<(), Error> {
    use std::path::Path;

    let target_binary_path = "/Library/PrivilegedHelperTools/io.github.clashverge.helper";
    let plist_file = "/Library/LaunchDaemons/io.github.clashverge.helper.plist";

    // Stop and unload service
    run_command("launchctl", &["stop", "io.github.clashverge.helper"], false)?;
    run_command("launchctl", &["bootout", "system", plist_file], false)?;
    run_command(
        "launchctl",
        &["disable", "system/io.github.clashverge.helper"],
        false,
    )?;

    // Remove files
    if Path::new(plist_file).exists() {
        std::fs::remove_file(plist_file)
            .map_err(|e| anyhow::anyhow!("Failed to remove plist file: {}", e))?;
    }

    if Path::new(target_binary_path).exists() {
        std::fs::remove_file(target_binary_path)
            .map_err(|e| anyhow::anyhow!("Failed to remove service binary: {}", e))?;
    }

    Ok(())
}

pub fn run_command(cmd: &str, args: &[&str], debug: bool) -> Result<(), Error> {
    if debug {
        println!("Executing: {} {}", cmd, args.join(" "));
    }

    let output = std::process::Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to execute '{}': {}", cmd, e))?;

    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if debug {
        eprintln!(
            "Command failed (status: {}):\nstdout: {}\nstderr: {}",
            output.status, stdout, stderr
        );
    }

    Err(anyhow::anyhow!(
        "Command '{}' failed (status: {}):\nstdout: {}\nstderr: {}",
        cmd,
        output.status,
        stdout,
        stderr
    ))
}

#[cfg(test)]
mod tests {
    use super::poll_until;
    use std::cell::Cell;

    #[test]
    fn poll_until_retries_transient_state_before_success() -> anyhow::Result<()> {
        let attempts = Cell::new(0);
        let pauses = Cell::new(0);

        let result = poll_until(
            3,
            || {
                let next = attempts.get() + 1;
                attempts.set(next);
                Ok((next == 3).then_some("deleted"))
            },
            || pauses.set(pauses.get() + 1),
            "service deletion timed out",
        )?;

        assert_eq!(result, "deleted");
        assert_eq!(attempts.get(), 3);
        assert_eq!(pauses.get(), 2);
        Ok(())
    }
}
