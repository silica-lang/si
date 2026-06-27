//
// Renode mock for Silica's abstract bus controller (std/i2c_controller.si,
// std/spi_controller.si) — the register-based CR/SR/SA/RA/DR protocol the metal
// `BusXfer` lowering drives (§3.5).  Renode does not model this abstract
// controller, so this peripheral supplies it for the trace-order parity check
// (harness/bus_parity.sh):
//
//   - a CR write (kick) clears status and lowers the IRQ, then, after a latency
//     (modelling a real in-flight transfer), sets DR + SR.done and raises the
//     completion IRQ.  The IRQ is wired to NVIC IRQ 8 (= __BUS_IRQN).
//
// Because completion is asynchronous, the CPU is free to run other reactions
// while the transfer is in flight — which is exactly the interleaving the
// IRQ-driven yields lowering (D2) produces and the busy-poll could not.
//
using System;
using Antmicro.Renode.Core;
using Antmicro.Renode.Logging;
using Antmicro.Renode.Peripherals.Bus;
using Antmicro.Renode.Time;

namespace Antmicro.Renode.Peripherals.Mocks
{
    public class MockBusController : IDoubleWordPeripheral, IKnownSize
    {
        public MockBusController(IMachine machine)
        {
            this.machine = machine;
            IRQ = new GPIO();
            Reset();
        }

        public long Size => 0x100;

        // Connect to the NVIC line the firmware enables per-transaction (IRQ 8).
        public GPIO IRQ { get; private set; }

        // Microseconds the transfer stays in flight before completing.  Long
        // enough (default 5ms) to leave an unambiguous window for a higher-
        // priority reaction to run during the suspension.
        public ulong LatencyMicroseconds { get; set; } = 5000;

        // Fault injection for the `match`-over-fault-codes gate (harness/
        // fault_match.sh, §4.4/D14): when nonzero, the next completion sets these
        // SR error bits instead of SR.done — modelling a real bus error
        // (nak=0x2, arblost=0x4, timeout=0x8, per std/i2c_controller.si).  The
        // firmware's resumed transaction decodes them into the matching arm.
        public uint FaultBits { get; set; } = 0;

        public void Reset()
        {
            cr = sr = sa = ra = dr = 0;
            IRQ.Unset();
        }

        public uint ReadDoubleWord(long offset)
        {
            switch(offset)
            {
                case CR: return cr;
                case SR: return sr;
                case SA: return sa;
                case RA: return ra;
                case DR: return dr;
                default: return 0;
            }
        }

        public void WriteDoubleWord(long offset, uint value)
        {
            switch(offset)
            {
                case CR:
                    cr = value;
                    sr = 0;          // transaction in flight
                    IRQ.Unset();     // lower any prior completion before re-arming
                    this.Log(LogLevel.Noisy, "kick (CR=0x{0:X}); completing in {1}us", value, LatencyMicroseconds);
                    machine.ScheduleAction(TimeInterval.FromMicroseconds(LatencyMicroseconds), _ => Complete());
                    break;
                case SA: sa = value; break;
                case RA: ra = value; break;
                case DR: dr = value; break;
                default: break;
            }
        }

        private void Complete()
        {
            if(FaultBits != 0)
            {
                dr = 0;
                sr = FaultBits;  // error completion (no SR.done) → firmware decodes the fault code
                this.Log(LogLevel.Noisy, "transfer FAULTED (SR=0x{0:X}), IRQ raised", FaultBits);
            }
            else
            {
                dr = 0x42;       // fixed read data (value is irrelevant to the parity check)
                sr = SR_DONE;    // SR.done, no error bits
                this.Log(LogLevel.Noisy, "transfer complete, IRQ raised");
            }
            IRQ.Set();           // raise the completion IRQ → firmware resumes the owner
        }

        private readonly IMachine machine;
        private uint cr, sr, sa, ra, dr;

        // Register offsets (match std/*_controller.si).
        private const long CR = 0x00;
        private const long SR = 0x04;
        private const long SA = 0x08;
        private const long RA = 0x0C;
        private const long DR = 0x10;
        private const uint SR_DONE = 0x1;
    }
}
