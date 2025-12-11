use anyhow::{Context, Result};
use std::env;
use std::process::Command;

pub fn start_service() -> Result<()> {
    let current_exe = env::current_exe()
        .context("Failed to get current executable path")?;
    
    let service_name = "boot-api";
    
    let output = Command::new("systemd-run")
        .args([
            "--unit", service_name,
            "--description", "Boot API Service",
            "--service-type", "exec",
            current_exe.to_str().unwrap(),
            "run-server"
        ])
        .output()
        .context("Failed to execute systemd-run command")?;
    
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to start systemd service: {}", stderr);
    }
    
    Ok(())
}


pub fn stop_service() -> Result<()> {
    let service_name = "boot-api";
    
    let output = Command::new("systemctl")
        .args(["stop", service_name])
        .output()
        .context("Failed to execute systemctl stop command")?;
    
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to stop systemd service: {}", stderr);
    }
    
    Ok(())
}

pub fn start_service_if_needed() -> Result<()> {
    let service_name = "boot-api";
    
    if is_service_active(service_name)? {
        return Ok(());
    }
    start_service()?;
    wait_until_service_active()?;
    
    Ok(())
}

fn is_service_active(service_name: &str) -> Result<bool> {
    let output = Command::new("systemctl")
        .args(["is-active", service_name])
        .output()
        .context("Failed to execute systemctl is-active command")?;
    
    Ok(output.status.success())
}

fn wait_until_service_active() -> Result<()> {
    let service_name = "boot-api";
    
    loop {
        if is_service_active(service_name)? {
            return Ok(());
        }
        
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
