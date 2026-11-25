// SPDX-License-Identifier: AGPL-3.0-or-later
//! Small SVD model used by the DAP peripheral view.

use std::path::Path;

use probe_rs::MemoryInterface;
use svd_parser::svd::{Device, Peripheral, Register, RegisterCluster};

#[derive(Debug, Clone)]
pub struct SvdModel {
    pub peripherals: Vec<SvdPeripheral>,
}

#[derive(Debug, Clone)]
pub struct SvdPeripheral {
    pub name: String,
    pub description: Option<String>,
    pub base_address: u64,
    pub registers: Vec<SvdRegister>,
}

#[derive(Debug, Clone)]
pub struct SvdRegister {
    pub name: String,
    pub description: Option<String>,
    pub address: u64,
    pub size: u32,
    pub fields: Vec<SvdField>,
}

#[derive(Debug, Clone)]
pub struct SvdField {
    pub name: String,
    pub offset: u32,
    pub width: u32,
}

impl SvdModel {
    pub fn parse(path: &Path) -> Result<Self, String> {
        let xml = std::fs::read_to_string(path)
            .map_err(|error| format!("failed to read SVD {}: {error}", path.display()))?;
        let device = svd_parser::parse(&xml)
            .map_err(|error| format!("failed to parse SVD {}: {error}", path.display()))?;
        Ok(Self::from_device(&device))
    }

    fn from_device(device: &Device) -> Self {
        let peripherals = device
            .peripherals
            .iter()
            .map(peripheral)
            .filter(|peripheral| !peripheral.registers.is_empty())
            .collect();
        Self { peripherals }
    }
}
fn peripheral(peripheral: &Peripheral) -> SvdPeripheral {
    let mut registers = Vec::new();
    if let Some(clusters) = &peripheral.registers {
        for cluster in clusters {
            register_cluster(cluster, peripheral.base_address, &mut registers);
        }
    }
    SvdPeripheral {
        name: peripheral.name.clone(),
        description: peripheral.description.clone(),
        base_address: peripheral.base_address,
        registers,
    }
}

fn register_cluster(cluster: &RegisterCluster, base: u64, out: &mut Vec<SvdRegister>) {
    match cluster {
        RegisterCluster::Register(register) => {
            if let Some(register) = svd_register(register, base) {
                out.push(register);
            }
        }
        RegisterCluster::Cluster(cluster) => {
            let cluster_base = base.saturating_add(u64::from(cluster.address_offset));
            for child in &cluster.children {
                register_cluster(child, cluster_base, out);
            }
        }
    }
}

fn svd_register(register: &Register, base: u64) -> Option<SvdRegister> {
    let fields = register
        .fields
        .as_ref()
        .map(|fields| {
            fields
                .iter()
                .map(|field| SvdField {
                    name: field.name.clone(),
                    offset: field.bit_range.offset,
                    width: field.bit_range.width,
                })
                .collect()
        })
        .unwrap_or_default();
    Some(SvdRegister {
        name: register.name.clone(),
        description: register.description.clone(),
        address: base.saturating_add(u64::from(register.address_offset)),
        size: register.properties.size.unwrap_or(32),
        fields,
    })
}

pub fn read_register(core: &mut impl MemoryInterface, register: &SvdRegister) -> String {
    match register.size {
        8 => core
            .read_word_8(register.address)
            .map(|value| format!("0x{value:02x}"))
            .unwrap_or_else(|error| format!("<unreadable: {error}>")),
        16 => core
            .read_word_16(register.address)
            .map(|value| format!("0x{value:04x}"))
            .unwrap_or_else(|error| format!("<unreadable: {error}>")),
        64 => core
            .read_word_64(register.address)
            .map(|value| format!("0x{value:016x}"))
            .unwrap_or_else(|error| format!("<unreadable: {error}>")),
        _ => core
            .read_word_32(register.address)
            .map(|value| format!("0x{value:08x}"))
            .unwrap_or_else(|error| format!("<unreadable: {error}>")),
    }
}
