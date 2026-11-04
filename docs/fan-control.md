# Fan speeds and control

Enable **Fans** in Settings, or drag its heading onto the desktop. Both
the panel and the desktop widget show each fan's measured RPM, including
zero for a stopped fan. A missing reading is `—`. GPU fans are read-only.

Click a supported fan's row to choose **Restore automatic**, **40%**,
**60%**, **80%** or **100%** output. These are output percentages, not
target RPM. Settings are never applied automatically after restarting.
The menu's check and the reading come from hardware acknowledgement.

## Adapters

| Interface | Read speed | Control |
|---|---|---|
| Redmi Book Pro 15 2022, TM2113, Ryzen 7 6800H, BIOS RMARB5B0P1111, IT5571 EC P.11 | Two independent RPM readings | One linked output for both fans; separate recovery process |
| Nuvoton NCT6779D, NCT6791/6792/6793/6795/6796/6797/6798/6799 | Existing Super I/O reader | Fixed PWM and restore the original automatic configuration |
| ITE IT8686/8688/8689/8721/8728/8792, except Gigabyte boards | Existing Super I/O reader | Fixed PWM and restore the original configuration, preserving other channels' shared bits |
| Other ITE, Nuvoton NCT6683/6686/6687, ASUS/Lenovo firmware readers | Existing reader, where available | Read-only |
| Other notebooks | Requires a documented and separately validated adapter | No guessed EC writes |

The controller protocols above do not guarantee every board's wiring or
firmware supports adjustment. Channels that have never reported a turning
fan, unknown chips, missing temperature readings and a channel already
under another application's manual control are not offered for takeover.
Only one hardware control adapter is registered by the sampler.

## Redmi adapter

This adapter uses the protocol already verified in
[RedmiFanControl](https://github.com/lajidonggua/RedmiFanControl).
Machine identity, chip ID, EC version, existing access permissions and PWM
configuration are checked. RPM comes from stable big-endian reads at
`0x0298/0x0299` and `0x02B8/0x02B9`. Writes are limited to the manual bit
of `0x0100` and linked targets `0x0105/0x0106`; hardware PWM registers and
access-permission bits are never written. The ISA bus lock is shared with
other hardware readers, and index selectors are restored after every access.

The original tool and Glance use the same control ownership mutex. If
RedmiFanControl is running, Glance can still show RPM but says **Another
controller**. Exit that tool normally before using Glance's adjustment menu;
there is no need to stop its processes forcibly.

At 80°C CPU Tctl, requested output becomes 100%. At 95°C, missing CPU or
RPM readings, a five-second stall or a lost UI connection, the worker
restores automatic. An independent `glance-sensors.exe` guardian holds the
ownership lock and a recovery ticket before the first manual write. It
also reads CPU temperature and RPM, restores after a four-second lost
worker lease or an eight-second stall, and handles an unexpected worker
exit. A fault cannot silently re-arm that guardian. Recovery changes only
the owned mode bit and targets; unrelated flags are preserved. If another
controller changes the targets, Glance relinquishes ownership instead of
overwriting them.

## Motherboard adapters

Register maps were checked against LibreHardwareMonitor
[Nct677X](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor/blob/v0.9.6/LibreHardwareMonitorLib/Hardware/Motherboard/Lpc/Nct677X.cs)
and [IT87XX](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor/blob/v0.9.6/LibreHardwareMonitorLib/Hardware/Motherboard/Lpc/IT87XX.cs).
The worker saves the original mode, PWM and channel enable bits before
writing, verifies writes, and restores those values when automatic is
selected or Glance exits normally. A disconnected UI also requests restore.
Partial writes and failed restores retain their recovery state for a retry.

At a maximum reported board temperature of 80°C, output becomes 100%; at
90°C, missing temperature, or five seconds without a turning fan, the
worker requests restore. These are board sensor readings, not an assurance
that every component's temperature is covered. Unlike the Redmi adapter,
these motherboard adapters do not yet have independent crash recovery. If
their sensor process is forcibly terminated, fixed output can remain until
the controller is reset or the machine restarts. Do not run two control
programs against the same fan headers.

## Validation and scope

Offline tests cover register writes, linked fan ordering, shared bits,
ownership conflicts, partial failure recovery, temperature/lease/stall
decisions, protocol compatibility and widget hit regions. Real hardware
results must be recorded separately; a mock test does not validate a new
motherboard or notebook firmware. NBFC and other notebook control
interfaces remain future adapters, rather than an assumption that one
laptop's EC layout works on another.

On 2026-11-05, the validated TM2113 machine was checked through the new
native adapter: both live RPM readings were available; linked 40% targets
were read back as `[51, 51]` with the manual bit set; normal restore cleared
the manual bit and both targets. A second bounded check exited the worker
without cleanup, after confirming manual output. A separate subsequent
hardware read confirmed that the guardian had cleared the mode bit and
both targets, and released ownership. No heat/load test or real stall was
induced. Other motherboard control adapters have only mock-register tests
in this change, rather than equivalent hardware validation.

The Redmi implementation is adapted from the MIT-licensed RedmiFanControl
protocol. Its [copyright and license](redmi-fan-control-LICENSE.txt) are
retained here. Existing PawnIO module licenses and corresponding source
remain in `pawnio-modules`.
