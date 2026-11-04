//! Fan control through identified motherboard controllers. No ports or
//! register addresses cross the UI/sensor-process boundary.
//!
//! Register maps checked against LibreHardwareMonitor v0.9.6:
//! Hardware/Motherboard/Lpc/{Nct677X,IT87XX}.cs. Each channel saves its
//! original settings, changes only its own bits, and relinquishes control
//! if another program changes the settings it owns.

use std::collections::BTreeMap;
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use crate::reading::{FanCommand, FanControl};

pub trait Registers {
    fn get(&self, address: u16) -> Result<u8, ()>;
    fn put(&self, address: u16, value: u8) -> Result<(), ()>;
}

#[derive(Clone, Copy)]
struct Plan {
    mode: u16,
    duty: u16,
    enable: Option<(u16, u8)>,
    manual: Option<u8>,
    auto_mask: u8,
}

#[derive(Clone)]
struct Saved {
    // Address, bits this channel owns, original value, last written value.
    bytes: Vec<(u16, u8, u8, u8)>,
    requested: u8,
    stalled: Option<Instant>,
    pending: Option<(usize, u8)>,
    restoring: bool,
}

#[derive(Default)]
pub struct Controller {
    plans: Vec<Plan>,
    owned: BTreeMap<u8, Saved>,
    pub failed: bool,
}

impl Controller {
    pub fn nuvoton(chip: u16, channels: usize) -> Self {
        if !matches!(chip & 0xfff8, 0xc560 | 0xc800 | 0xc910 | 0xd120 | 0xd350 | 0xd420 | 0xd450 | 0xd428 | 0xd800) {
            return Self::default();
        }
        let banks = [0x100, 0x200, 0x300, 0x800, 0x900, 0xa00, 0xb00];
        Self { plans: banks.into_iter().take(channels.min(7)).map(|bank| Plan {
            mode: bank + 2, duty: bank + 9, enable: None, manual: Some(0), auto_mask: 0xff,
        }).collect(), ..Self::default() }
    }

    pub fn ite(chip: u16, channels: usize, vendor: &str) -> Self {
        // Gigabyte may put another EC in charge of these registers. Its
        // takeover protocol is separate; keep those boards read-only here.
        if vendor.to_ascii_lowercase().contains("gigabyte") || !matches!(chip, 0x8686 | 0x8688 | 0x8689 | 0x8721 | 0x8728 | 0x8792) {
            return Self::default();
        }
        let mode = [0x15, 0x16, 0x17, 0x7f, 0xa7, 0xaf];
        let duty = [0x63, 0x6b, 0x73, 0x7b, 0xa3, 0xab];
        Self { plans: (0..channels.min(6)).map(|i| Plan {
            mode: mode[i], duty: duty[i], enable: (i < 3).then_some((0x13, 1 << i)),
            manual: (chip == 0x8689).then_some(0x7f), auto_mask: 0x80,
        }).collect(), ..Self::default() }
    }

    pub fn available(&self) -> bool { !self.plans.is_empty() }

    pub fn relinquish(&mut self, command: FanCommand) {
        for (&channel, saved) in &mut self.owned {
            if matches!(command, FanCommand::RestoreAll) || matches!(command, FanCommand::Automatic(c) if c == channel) { saved.restoring = true; }
        }
    }

    fn agrees(io: &impl Registers, saved: &mut Saved) -> Result<bool, ()> {
        // An I/O error may arrive after the device accepted a write. Resolve
        // that one uncertain byte before comparing ownership, including on a retry.
        if let Some((at, value)) = saved.pending {
            let (reg, mask, _, last) = saved.bytes[at];
            let read = io.get(reg)? & mask;
            if read == value & mask { saved.bytes[at].3 = value; }
            else if read != last & mask { return Ok(false); }
            saved.pending = None;
        }
        for &(reg, mask, _, last) in &saved.bytes {
            if io.get(reg)? & mask != last & mask { return Ok(false); }
        }
        Ok(true)
    }

    fn write(io: &impl Registers, saved: &mut Saved, at: usize, value: u8) -> Result<(), ()> {
        let (reg, mask, _, _) = saved.bytes[at];
        let next = (io.get(reg)? & !mask) | (value & mask);
        saved.pending = Some((at, value));
        io.put(reg, next)?;
        if io.get(reg)? & mask != next & mask { return Err(()); }
        saved.bytes[at].3 = value;
        saved.pending = None;
        Ok(())
    }

    pub fn restore(&mut self, io: &impl Registers, channel: u8) -> Result<(), ()> {
        let Some(saved) = self.owned.get_mut(&channel) else { return Ok(()) };
        saved.restoring = true;
        if !Self::agrees(io, saved)? {
            self.owned.remove(&channel);
            self.failed = true;
            return Err(());
        }
        // The saved automatic mode first, then its PWM and enable bits.
        for at in 0..saved.bytes.len() {
            let original = saved.bytes[at].2;
            Self::write(io, saved, at, original)?;
        }
        self.owned.remove(&channel);
        Ok(())
    }

    pub fn restore_all(&mut self, io: &impl Registers) -> bool {
        let channels: Vec<u8> = self.owned.keys().copied().collect();
        let mut okay = true;
        for channel in channels { okay &= self.restore(io, channel).is_ok(); }
        okay
    }

    pub fn apply(&mut self, io: &impl Registers, command: FanCommand, active: u32, temperature: Option<f32>) -> Result<(), ()> {
        match command {
            FanCommand::RestoreAll => {
                if self.restore_all(io) { self.failed = false; return Ok(()); }
                return Err(());
            }
            FanCommand::Automatic(channel) => {
                self.restore(io, channel)?;
                self.failed = false;
                return Ok(());
            }
            FanCommand::Fixed { channel, percent } => {
                if !(40..=100).contains(&percent) || channel >= 32 || active & (1 << channel) == 0 { return Err(()); }
                let temperature = temperature.filter(|t| t.is_finite() && *t > 0.0 && *t < 90.0).ok_or(())?;
                let plan = *self.plans.get(channel as usize).ok_or(())?;
                if let Some(saved) = self.owned.get_mut(&channel) {
                    if saved.restoring { return Err(()); }
                    if !Self::agrees(io, saved)? {
                        self.owned.remove(&channel);
                        return Err(());
                    }
                } else {
                    let mode = io.get(plan.mode)?;
                    // Do not take a channel already in somebody else's manual mode.
                    if mode & plan.auto_mask == 0 { return Err(()); }
                    let pwm = io.get(plan.duty)?;
                    let mut bytes = vec![(plan.mode, 0xff, mode, mode), (plan.duty, 0xff, pwm, pwm)];
                    if let Some((reg, mask)) = plan.enable { let value = io.get(reg)?; bytes.push((reg, mask, value, value)); }
                    self.owned.insert(channel, Saved { bytes, requested: percent, stalled: None, pending: None, restoring: false });
                }
                let saved = self.owned.get_mut(&channel).unwrap();
                saved.requested = percent;
                let duty = ((if temperature >= 80.0 { 100 } else { percent }) as u32 * 255 + 50) / 100;
                let desired = [plan.manual.unwrap_or(saved.bytes[0].2 & 0x7f), duty as u8, plan.enable.map_or(0, |(_, mask)| mask)];
                // Remember a byte immediately after writing it, so a failed
                // later write can still restore the earlier ones.
                for (i, &(_, mask, _, last)) in saved.bytes.clone().iter().enumerate() {
                    if last & mask == desired[i] & mask { continue; }
                    Self::write(io, saved, i, desired[i])?;
                }
            }
        }
        Ok(())
    }

    pub fn maintain(&mut self, io: &impl Registers, active: u32, temperature: Option<f32>, rpms: impl Fn(u8) -> Option<f32>, now: Instant) {
        if self.owned.is_empty() { return; }
        if temperature.is_none_or(|t| !t.is_finite() || t >= 90.0 || t <= 0.0) {
            self.restore_all(io);
            self.failed = true;
            return;
        }
        let channels: Vec<u8> = self.owned.keys().copied().collect();
        for channel in channels {
            let saved = self.owned.get_mut(&channel).unwrap();
            if saved.restoring {
                let _ = self.restore(io, channel);
                self.failed = true;
                continue;
            }
            let stalled = if rpms(channel).is_some_and(|rpm| rpm > 0.0) { saved.stalled = None; false }
                else { now.duration_since(*saved.stalled.get_or_insert(now)) >= Duration::from_secs(5) };
            let request = saved.requested;
            if stalled || self.apply(io, FanCommand::Fixed { channel, percent: request }, active, temperature).is_err() {
                let _ = self.restore(io, channel);
                self.failed = true;
            }
        }
    }

    pub fn readings(&self, io: &impl Registers, active: u32, temperature: Option<f32>, name: impl Fn(u8) -> String) -> Vec<FanControl> {
        self.plans.iter().enumerate().filter(|(i, _)| active & (1 << i) != 0).map(|(i, plan)| {
            let owned = self.owned.get(&(i as u8));
            let mode = io.get(plan.mode).ok();
            FanControl { channel: i as u8, name: name(i as u8), percent: owned.and_then(|_| io.get(plan.duty).ok()).map(|v| ((v as u32 * 100 + 127) / 255) as u8),
                manual: owned.is_some(), available: temperature.is_some_and(|t| t.is_finite() && t > 0.0 && t < 90.0) && mode.is_some_and(|m| owned.is_some() || m & plan.auto_mask != 0), linked: false }
        }).collect()
    }
}

pub struct Task { pub command: FanCommand, pub reply: mpsc::SyncSender<bool>, pub until: Instant }
static WORKER: Mutex<Option<mpsc::Sender<Task>>> = Mutex::new(None);

pub fn register(worker: mpsc::Sender<Task>) { *WORKER.lock().unwrap() = Some(worker); }

pub fn request(command: FanCommand) -> bool {
    request_within(command, Duration::from_secs(3))
}

pub fn request_within(command: FanCommand, within: Duration) -> bool {
    let Some(worker) = WORKER.lock().unwrap().clone() else { return false };
    let (reply, answer) = mpsc::sync_channel(1);
    worker.send(Task { command, reply, until: Instant::now() + within }).is_ok() && answer.recv_timeout(within).unwrap_or(false)
}

/// Build choices only from a fresh sensor capability, never a guessed chip
/// address or a remembered manual setting from an earlier run.
pub fn menu(channel: u8, at: windows::Win32::Foundation::POINT, with_panel: bool, look: crate::ui::menu::Look) -> Option<(crate::ui::menu::Menu, Vec<FanCommand>)> {
    use crate::ui::menu::{Item, Menu};
    let control = crate::app().controller.history.read().unwrap().back()?.board.as_ref()?.fan_controls.iter().find(|c| c.channel == channel)?.clone();
    let lang = crate::update::language();
    let mut rows = vec![(FanCommand::Automatic(channel), lang.pick("恢复自动", "Restore automatic"), !control.manual)];
    if control.available {
        let labels = if control.linked { [(40, lang.pick("40% · 两路联动", "40% · Linked fans")), (60, lang.pick("60% · 两路联动", "60% · Linked fans")), (80, lang.pick("80% · 两路联动", "80% · Linked fans")), (100, lang.pick("100% · 两路联动", "100% · Linked fans"))] }
            else { [(40, "40%"), (60, "60%"), (80, "80%"), (100, "100%")] };
        rows.extend(labels.into_iter().map(|(percent, label)|
            (FanCommand::Fixed { channel, percent }, label, control.manual && control.percent == Some(percent))));
    } else if !control.manual { return None; }
    let items = rows.iter().enumerate().map(|(i, (_, label, checked))| Item { label, checked: *checked, mark: None, rule_before: i == 1 }).collect();
    Some((Menu::open_at(items, look, at, false, with_panel)?, rows.into_iter().map(|(command, ..)| command).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    struct Fake(RefCell<BTreeMap<u16, u8>>);
    impl Registers for Fake {
        fn get(&self, r: u16) -> Result<u8, ()> { self.0.borrow().get(&r).copied().ok_or(()) }
        fn put(&self, r: u16, v: u8) -> Result<(), ()> { self.0.borrow_mut().insert(r, v); Ok(()) }
    }
    #[test]
    fn restores_original_mode_and_pwm_and_rejects_unknown_channels() {
        let io = Fake(RefCell::new(BTreeMap::from([(0x102, 0x40), (0x109, 180)])));
        let mut c = Controller::nuvoton(0xd428, 7);
        assert!(c.apply(&io, FanCommand::Fixed { channel: 7, percent: 40 }, 1, Some(45.0)).is_err());
        assert!(c.apply(&io, FanCommand::Fixed { channel: 0, percent: 0 }, 1, Some(45.0)).is_err());
        c.apply(&io, FanCommand::Fixed { channel: 0, percent: 40 }, 1, Some(45.0)).unwrap();
        assert_eq!((io.get(0x102), io.get(0x109)), (Ok(0), Ok(102)));
        assert!(c.restore_all(&io));
        assert_eq!((io.get(0x102), io.get(0x109)), (Ok(0x40), Ok(180)));
    }
    #[test]
    fn never_overwrites_another_controllers_changes() {
        let io = Fake(RefCell::new(BTreeMap::from([(0x102, 0x40), (0x109, 180)])));
        let mut c = Controller::nuvoton(0xd428, 7);
        c.apply(&io, FanCommand::Fixed { channel: 0, percent: 40 }, 1, Some(45.0)).unwrap();
        io.put(0x109, 200).unwrap();
        assert!(!c.restore_all(&io));
        assert_eq!(io.get(0x109), Ok(200));
        assert!(c.owned.is_empty());
    }
    #[test]
    fn shared_ite_bits_are_preserved_and_temperature_loss_restores_automatic() {
        let io = Fake(RefCell::new(BTreeMap::from([(0x15, 0x80), (0x63, 180), (0x13, 0xa0)])));
        let mut c = Controller::ite(0x8686, 6, "ASRock");
        c.apply(&io, FanCommand::Fixed { channel: 0, percent: 40 }, 1, Some(85.0)).unwrap();
        assert_eq!(io.get(0x63), Ok(255));
        io.put(0x13, 0xa3).unwrap(); // A different channel's enable bit.
        c.maintain(&io, 1, None, |_| Some(1200.0), Instant::now());
        assert_eq!((io.get(0x13), io.get(0x15), io.get(0x63)), (Ok(0xa2), Ok(0x80), Ok(180)));
        assert!(!Controller::ite(0x8686, 6, "Gigabyte").available());
        assert!(!Controller::nuvoton(0xd590, 8).available());
    }

    #[test]
    fn heat_overrides_output_and_a_stalled_fan_returns_to_firmware() {
        let io = Fake(RefCell::new(BTreeMap::from([(0x102, 0x40), (0x109, 180)])));
        let mut c = Controller::nuvoton(0xd428, 7);
        let now = Instant::now();
        c.apply(&io, FanCommand::Fixed { channel: 0, percent: 60 }, 1, Some(45.0)).unwrap();
        c.maintain(&io, 1, Some(81.0), |_| Some(1200.0), now);
        assert_eq!(io.get(0x109), Ok(255));
        c.maintain(&io, 1, Some(70.0), |_| Some(1200.0), now);
        assert_eq!(io.get(0x109), Ok(153));
        c.maintain(&io, 1, Some(70.0), |_| Some(0.0), now);
        c.maintain(&io, 1, Some(70.0), |_| None, now + Duration::from_secs(5));
        assert_eq!((io.get(0x102), io.get(0x109)), (Ok(0x40), Ok(180)));
        assert!(c.failed && c.owned.is_empty());
        for t in [None, Some(f32::NAN), Some(90.0), Some(0.0)] {
            assert!(c.apply(&io, FanCommand::Fixed { channel: 0, percent: 40 }, 1, t).is_err());
        }
    }

    #[test]
    fn a_partial_write_or_restore_can_be_retried_without_losing_originals() {
        use std::cell::Cell;
        struct Flaky { io: Fake, fail: Cell<Option<u16>>, after: bool }
        impl Registers for Flaky {
            fn get(&self, r: u16) -> Result<u8, ()> { self.io.get(r) }
            fn put(&self, r: u16, v: u8) -> Result<(), ()> {
                if self.fail.get() == Some(r) {
                    self.fail.set(None);
                    if self.after { self.io.put(r, v)?; }
                    return Err(());
                }
                self.io.put(r, v)
            }
        }
        for after in [false, true] {
            let io = Flaky { io: Fake(RefCell::new(BTreeMap::from([(0x102, 0x40), (0x109, 180)]))), fail: Cell::new(Some(0x109)), after };
            let mut c = Controller::nuvoton(0xd428, 7);
            assert!(c.apply(&io, FanCommand::Fixed { channel: 0, percent: 40 }, 1, Some(45.0)).is_err());
            assert!(c.restore_all(&io));
            assert_eq!((io.get(0x102), io.get(0x109)), (Ok(0x40), Ok(180)));
            c.apply(&io, FanCommand::Fixed { channel: 0, percent: 40 }, 1, Some(45.0)).unwrap();
            io.fail.set(Some(0x109));
            assert!(!c.restore_all(&io));
            assert!(c.owned.get(&0).unwrap().restoring);
            // Maintenance finishes restoring; it must not re-enter manual mode.
            c.maintain(&io, 1, Some(45.0), |_| Some(1200.0), Instant::now());
            assert_eq!((io.get(0x102), io.get(0x109)), (Ok(0x40), Ok(180)));
            assert!(c.owned.is_empty());
        }
    }

    #[test]
    fn stopping_one_ite_channel_preserves_the_other_channel() {
        let io = Fake(RefCell::new(BTreeMap::from([(0x15, 0x80), (0x63, 180), (0x16, 0x80), (0x6b, 190), (0x13, 0xa0)])));
        let mut c = Controller::ite(0x8686, 6, "ASRock");
        for channel in [0, 1] { c.apply(&io, FanCommand::Fixed { channel, percent: 60 }, 3, Some(45.0)).unwrap(); }
        c.apply(&io, FanCommand::Automatic(0), 3, None).unwrap();
        assert_eq!((io.get(0x13), io.get(0x16), io.get(0x6b)), (Ok(0xa2), Ok(0), Ok(153)));
        assert!(c.restore_all(&io));
        assert_eq!(io.get(0x13), Ok(0xa0));
    }

    #[test]
    fn older_readings_are_read_only_and_commands_round_trip() {
        let board: crate::reading::BoardSensors = serde_json::from_str(r#"{"temps":[],"fans":[["1",1200.0]]}"#).unwrap();
        assert!(board.fan_controls.is_empty() && !board.fan_control_failed);
        for command in [FanCommand::RestoreAll, FanCommand::Automatic(2), FanCommand::Fixed { channel: 1, percent: 80 }] {
            let json = serde_json::to_string(&command).unwrap();
            assert_eq!(serde_json::to_string(&serde_json::from_str::<FanCommand>(&json).unwrap()).unwrap(), json);
        }
    }
}
