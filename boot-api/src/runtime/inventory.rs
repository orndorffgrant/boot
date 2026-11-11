use std::collections::HashSet;

use anyhow::Result;

use crate::runtime::{RuntimeInner, stable_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Probe,
    SerialPort,
}

#[derive(Debug, Clone)]
pub struct ResourceEntry {
    pub id: String,
    pub kind: ResourceKind,
    pub name: String,
    pub selector: Option<String>,
    pub path: Option<String>,
    pub generation: u64,
    pub state: String,
    pub identity_stable: bool,
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    pub serial_number: Option<String>,
    pub probe_type: Option<String>,
}

pub async fn refresh(inner: &RuntimeInner) -> Result<()> {
    let (probes, ports) = tokio::task::spawn_blocking(|| {
        let lister = probe_rs::probe::list::Lister::new();
        let probes = lister.list_all();
        let ports = serialport::available_ports().unwrap_or_default();
        (probes, ports)
    })
    .await?;

    let mut resources = inner.resources.lock().expect("resources lock");
    let mut seen = HashSet::new();
    for probe in probes {
        let serial = probe.serial_number.clone().unwrap_or_default();
        let selector = format!(
            "{:04x}:{:04x}:{}",
            probe.vendor_id, probe.product_id, serial
        );
        let identity = format!("probe:{}:{}:{}", probe.vendor_id, probe.product_id, serial);
        let stable = !serial.is_empty();
        let id = stable_id("probe", &identity);
        seen.insert(id.clone());
        let generation = resources
            .get(&id)
            .map(|resource| resource.generation)
            .unwrap_or_else(|| {
                inner
                    .generation
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            });
        resources.insert(
            id.clone(),
            ResourceEntry {
                id,
                kind: ResourceKind::Probe,
                name: probe.identifier.clone(),
                selector: Some(selector),
                path: None,
                generation,
                state: "present".to_string(),
                identity_stable: stable,
                vendor_id: Some(probe.vendor_id),
                product_id: Some(probe.product_id),
                serial_number: probe.serial_number.clone(),
                probe_type: Some(probe.probe_type()),
            },
        );
    }

    for port in ports {
        if !matches!(port.port_type, serialport::SerialPortType::UsbPort(_)) {
            continue;
        }
        let (vid, pid, serial) = match &port.port_type {
            serialport::SerialPortType::UsbPort(info) => {
                (Some(info.vid), Some(info.pid), info.serial_number.clone())
            }
            _ => (None, None, None),
        };
        let identity = format!(
            "port:{}:{}:{}:{}",
            vid.unwrap_or(0),
            pid.unwrap_or(0),
            serial.clone().unwrap_or_default(),
            port.port_name
        );
        let id = stable_id("port", &identity);
        seen.insert(id.clone());
        let generation = resources
            .get(&id)
            .map(|resource| resource.generation)
            .unwrap_or_else(|| {
                inner
                    .generation
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            });
        let name = match &port.port_type {
            serialport::SerialPortType::UsbPort(info) => Some(format!(
                "{} ({})",
                info.product.as_deref().unwrap_or("USB Serial"),
                info.serial_number.as_deref().unwrap_or("no serial")
            )),
            _ => None,
        };
        resources.insert(
            id.clone(),
            ResourceEntry {
                id,
                kind: ResourceKind::SerialPort,
                name: name.unwrap_or_else(|| port.port_name.clone()),
                selector: None,
                path: Some(port.port_name.clone()),
                generation,
                state: "present".to_string(),
                identity_stable: serial.is_some(),
                vendor_id: vid,
                product_id: pid,
                serial_number: serial,
                probe_type: None,
            },
        );
    }

    resources.retain(|id, _| seen.contains(id));
    Ok(())
}
