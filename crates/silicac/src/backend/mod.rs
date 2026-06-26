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
                "-O1",
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
