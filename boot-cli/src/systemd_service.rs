use anyhow::{Context, Result};
use std::env;
use std::process::Command;

pub fn start_service(host: Option<String>, port: Option<u16>) -> Result<()> {
    let current_exe = env::current_exe()
        .context("Failed to get current executable path")?;
    
    let service_name = "boot-api";
    
    // Create owned strings that live long enough
    let port_string = port.map(|p| p.to_string());
    
    let mut args = vec![
        "--unit", service_name,
        "--description", "Boot API Service",
        "--service-type", "exec",
        current_exe.to_str().unwrap(),
        "run-server",
    ];
    
    if let Some(ref host) = host {
        args.push("--host");
        args.push(host.as_str());
    }
    
    if let Some(ref port_str) = port_string {
        args.push("--port");
        args.push(port_str.as_str());
    }
    
    let output = Command::new("systemd-run")
        .args(args)
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
    start_service(None, None)?;
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
