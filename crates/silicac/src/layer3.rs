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

/// One entry of the PC→(handler, `when`-state) **site map** (§5.4/§7.2, P7-4a):
/// which reaction handler owns a code region, and the device typestate it runs
/// under.  Emitted as a table so a fault-time PC can be attributed to a handler
/// (the decode + wire-up is P7-4b).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteEntry {
    /// The reaction id (also the `__reaction_<id>` handler symbol).
    pub reaction_id: usize,
    /// The generated handler function symbol, e.g. `__reaction_1`.
    pub handler: String,
    /// A human-readable trigger description (`sys.start`, `every 100ms`, …).
    pub trigger: String,
    /// The device typestate the handler runs under: `(device name, state)`.
    pub when_state: Vec<(String, String)>,
}

/// The generated handler function symbol for a reaction id.
pub fn handler_symbol(reaction_id: usize) -> String {
    format!("__reaction_{reaction_id}")
}

/// A human-readable description of a reaction trigger, for the site map.
fn trigger_desc(t: &SirTrigger) -> String {
    match t {
        SirTrigger::SysStart => "sys.start".to_string(),
        SirTrigger::EveryNs(ns) => format!("every {ns}ns"),
        SirTrigger::Event(id) => format!("event #{id}"),
    }
}

/// Build the Layer-3 site map from the module: one entry per reaction handler,
/// carrying its trigger and the device typestate it provably runs under
/// (§5.4/§7.2, P7-4a).  Device ids on each reaction's `when_state` are resolved
/// to instance names via the module's device list.
pub fn site_map(module: &SirModule) -> Vec<SiteEntry> {
    let dev_name = |id: usize| {
        module
            .devices
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| format!("device#{id}"))
    };
    module
        .reactions
        .iter()
        .map(|r| SiteEntry {
            reaction_id: r.id,
            handler: handler_symbol(r.id),
            trigger: trigger_desc(&r.trigger),
            when_state: r.when_state.iter().map(|(d, s)| (dev_name(*d), s.clone())).collect(),
        })
        .collect()
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

    fn reaction(id: usize, trigger: SirTrigger, when_state: Vec<(usize, String)>) -> SirReaction {
        SirReaction {
            id,
            trigger,
            body: vec![],
            priority: 0,
            disposition: SirDisposition::Escalate,
            yields: false,
            deadline_ns: None,
            overflow: SirOverflow::Coalesce,
            when_state,
        }
    }

    #[test]
    fn site_map_maps_handlers_triggers_and_when_state() {
        // P7-4a: one entry per reaction, with its `__reaction_<id>` handler, a
        // trigger description, and its device typestate resolved to names.
        let mut m = module();
        m.reactions = vec![
            reaction(0, SirTrigger::SysStart, vec![]),
            reaction(1, SirTrigger::EveryNs(100_000_000), vec![(0, "configured".into())]),
        ];
        let sites = site_map(&m);
        assert_eq!(sites.len(), 2);
        assert_eq!(sites[0].handler, "__reaction_0");
        assert_eq!(sites[0].trigger, "sys.start");
        assert!(sites[0].when_state.is_empty());
        assert_eq!(sites[1].handler, "__reaction_1");
        assert_eq!(sites[1].trigger, "every 100000000ns");
        // device id 0 → its instance name `gpio0`.
        assert_eq!(sites[1].when_state, vec![("gpio0".to_string(), "configured".to_string())]);
    }
}
