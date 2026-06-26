pub mod c;
pub mod stackinfo;

/// Which target a backend lowering is for.  Both targets are *consumers* of the
/// same SIR (§6.1); only the printer differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Freestanding host program (Linux/macOS/Windows) — the existing path used
    /// by the simulator's C sibling and by `hello`/`blink`.
    Host,
    /// Bare-metal nRF52840 (Cortex-M4F): generated vector table, reset/startup,
    /// linker script, no libc (§6.2/§6.4).
    MetalNrf52840,
}

impl Target {
    /// The C compiler that should build this target's output by default.
    pub fn default_cc(self) -> &'static str {
        match self {
            Target::Host => "cc",
            Target::MetalNrf52840 => "arm-none-eabi-gcc",
        }
    }

    /// Extra compiler flags for this target.
    pub fn cc_flags(self) -> &'static [&'static str] {
        match self {
            Target::Host => &[],
            Target::MetalNrf52840 => &[
                "-mcpu=cortex-m4",
                "-mthumb",
                "-ffreestanding",
                "-nostdlib",
                "-nostartfiles",
                // Embedded default: optimise for size (flash is the scarce
                // resource); overridable with `--opt <level>` (audit #35 P1-2).
                "-Os",
                // Emit the toolchain's own stack accounting so the RAM budget
                // can be a *measured* bound, not a synthetic estimate (audit #35,
                // §5.3): `.ci` (per-function frames + call edges) and `.su`
                // (per-function frames) — parsed by `backend::stackinfo`.
                "-fstack-usage",
                "-fcallgraph-info=su,da",
            ],
        }
    }
}

/// Resolve a `--opt <level>` CLI override into a `-O…` flag (audit #35 P1-2).
/// Accepts a bare level (`s`, `2`, `z`, `0`…) or a full flag (`-O2`); `None`
/// means "use the target default already in `cc_flags()`".
pub fn opt_override_flag(opt: Option<&str>) -> Option<String> {
    opt.map(|o| {
        let o = o.trim();
        if o.starts_with("-O") {
            o.to_string()
        } else {
            format!("-O{o}")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metal_defaults_to_size_opt() {
        assert!(Target::MetalNrf52840.cc_flags().contains(&"-Os"), "metal default is -Os");
        assert!(!Target::MetalNrf52840.cc_flags().contains(&"-O1"));
    }

    #[test]
    fn opt_override_forms_a_flag() {
        assert_eq!(opt_override_flag(Some("s")).as_deref(), Some("-Os"));
        assert_eq!(opt_override_flag(Some("2")).as_deref(), Some("-O2"));
        assert_eq!(opt_override_flag(Some("-O3")).as_deref(), Some("-O3"));
        assert_eq!(opt_override_flag(None), None);
    }
}
