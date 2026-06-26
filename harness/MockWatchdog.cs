//
// Renode mock for Silica's abstract system watchdog (std/watchdog.si) — the
// CR/RLR/KR protocol the metal backend drives (§5.6).  Renode's nRF52840 models
// a *real* WDT with a different register layout, so the parity harness
// unregisters it and loads this in its place.
//
//   - RLR (0x04) write sets the reload, in milliseconds.
//   - CR  (0x00) write with bit 0 set starts the countdown.
//   - KR  (0x08) write feeds it (re-arms the countdown).
//   - SR  (0x0C) read returns 1 once it has expired unfed (the harness observes
//     this as "the watchdog would have reset the system").
//
// While the firmware keeps feeding (its scheduler returns to idle), the timer is
// continually re-armed and never expires.  If a reaction wedges and the idle
// loop stops feeding, the last-armed timer elapses and `fired` latches.
//
using System;
using Antmicro.Renode.Core;
using Antmicro.Renode.Logging;
using Antmicro.Renode.Peripherals.Bus;
using Antmicro.Renode.Time;

namespace Antmicro.Renode.Peripherals.Mocks
{
    public class MockWatchdog : IDoubleWordPeripheral, IKnownSize
    {
        public MockWatchdog(IMachine machine)
        {
            this.machine = machine;
            Reset();
        }

        public long Size => 0x100;

        public void Reset()
        {
            reloadMs = 0;
            started = false;
            fired = 0;
            armGeneration = 0;
        }

        public uint ReadDoubleWord(long offset)
        {
            switch(offset)
            {
                case SR: return fired;   // 1 once expired unfed
                default: return 0;
            }
        }

        public void WriteDoubleWord(long offset, uint value)
        {
            switch(offset)
            {
                case CR:
                    if((value & 0x1u) != 0)
                    {
                        started = true;
                        Arm();
                    }
                    break;
                case RLR:
                    reloadMs = value;
                    break;
                case KR:
                    if(started)
                    {
                        Arm(); // feed: push the expiry out
                    }
                    break;
                default:
                    break;
            }
        }

        private void Arm()
        {
            // Bump the generation so any earlier pending expiry is a no-op; only
            // the most recent arming can latch `fired`.
            armGeneration++;
            var gen = armGeneration;
            var ms = reloadMs == 0 ? 1u : reloadMs;
            machine.ScheduleAction(TimeInterval.FromMilliseconds(ms), _ =>
            {
                if(started && gen == armGeneration)
                {
                    fired = 1;
                    this.Log(LogLevel.Warning, "watchdog expired unfed — system would reset (§5.6)");
                }
            });
        }

        private readonly IMachine machine;
        private uint reloadMs;
        private bool started;
        private uint fired;
        private ulong armGeneration;

        private const long CR = 0x00;
        private const long RLR = 0x04;
        private const long KR = 0x08;
        private const long SR = 0x0C;
    }
}
