//! Layer-3 fault decoding (§4.4/§5.4).
//!
//! A hardware trap (HardFault / bus fault) carries a faulting *address*; on its
//! own that is opaque.  The compiler knows, from the board, which device claims
//! each MMIO range and where flash/RAM live — the **address-ownership map**.
//! The decoder turns a faulting address back into a language-level diagnosis:
//! *"store to 0x4002_0000 — no device claims this address"*, or *"…within
//! `gpio0` (0x5000_0000), likely an access/when-state violation"* (§5.4).
//!
//! The same map is consumed by the host simulator (to decode injected faults)
//! and by the generated metal `HardFault_Handler` (emitted as a C table), so the
//! two agree by construction (§7.2).

use crate::sir::*;

/// A claimed address range and what owns it.
#[derive(Debug, Clone)]
pub struct OwnedRegion {
    pub start: u64,
    pub end: u64, // exclusive
    pub label: String,
    pub kind: RegionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionKind {
    /// A device's MMIO register block.
    Mmio,
    /// A flash / RAM memory region.
    Memory,
}

/// Build the address-ownership map from the board: each device instance's MMIO
/// register span, plus the SoC memory regions (§5.4).
pub fn ownership_map(module: &SirModule) -> Vec<OwnedRegion> {
    let mut regions = Vec::new();

    for dev in &module.devices {
        let Some(base) = dev.base_addr else { continue };
        // Span = past the last declared register (offset + its width in bytes).
        let span = dev
            .regs
            .iter()
            .map(|r| r.offset + (r.width as u64 / 8))
            .max()
            .unwrap_or(4);
        regions.push(OwnedRegion {
            start: base,
            end: base + span,
            label: dev.name.clone(),
            kind: RegionKind::Mmio,
        });
    }

    for region in &module.memory {
        regions.push(OwnedRegion {
            start: region.origin,
            end: region.origin + region.size,
            label: region.name.clone(),
            kind: RegionKind::Memory,
        });
    }

    regions.sort_by_key(|r| r.start);
    regions
}

/// The result of decoding a faulting address against the ownership map.
#[derive(Debug, Clone)]
pub struct FaultDecode {
    pub address: u64,
    pub owner: Option<OwnedRegion>,
    /// A language-level diagnosis, rendered host-side (no on-device strings).
    pub diagnosis: String,
}

/// Decode a faulting address (§5.4): find its owning region and produce a
/// language-level diagnosis.
pub fn decode_address(module: &SirModule, addr: u64) -> FaultDecode {
    let owner = ownership_map(module)
        .into_iter()
        .find(|r| addr >= r.start && addr < r.end);
    let diagnosis = match &owner {
        Some(r) if r.kind == RegionKind::Mmio => format!(
            "fault at 0x{:08x} — within device `{}` (base 0x{:08x}); likely an access or when-state violation",
            addr, r.label, r.start
        ),
        Some(r) => format!(
            "fault at 0x{:08x} — within `{}` memory region",
            addr, r.label
        ),
        None => format!("fault at 0x{:08x} — no device claims this address", addr),
    };
    FaultDecode { address: addr, owner, diagnosis }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: usize, name: &str, base: u64, regs: &[(u64, u8)]) -> SirDevice {
        SirDevice {
            id,
            name: name.into(),
            base_addr: Some(base),
            kind: SirDeviceKind::Generic,
            regs: regs
                .iter()
                .map(|(off, w)| SirReg {
                    name: "r".into(),
                    offset: *off,
                    width: *w,
                    access: SirRegAccess::Rw,
                    reset: 0,
                })
                .collect(),
        }
    }

    fn module() -> SirModule {
        SirModule {
            devices: vec![dev(0, "gpio0", 0x5000_0000, &[(0x504, 32), (0x514, 32)])],
            memory: vec![
                SirRegion { name: "flash".into(), origin: 0x0, size: 0x10_0000 },
                SirRegion { name: "ram".into(), origin: 0x2000_0000, size: 0x4_0000 },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn unclaimed_address_is_diagnosed() {
        let d = decode_address(&module(), 0x4001_0000);
        assert!(d.owner.is_none());
        assert!(d.diagnosis.contains("no device claims this address"));
    }

    #[test]
    fn device_mmio_is_attributed() {
        let d = decode_address(&module(), 0x5000_0514);
        assert_eq!(d.owner.as_ref().unwrap().label, "gpio0");
        assert!(d.diagnosis.contains("within device `gpio0`"));
    }

    #[test]
    fn memory_region_is_attributed() {
        let d = decode_address(&module(), 0x2000_0010);
        assert_eq!(d.owner.as_ref().unwrap().kind, RegionKind::Memory);
    }
}
