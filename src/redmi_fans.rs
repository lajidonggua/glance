//! TM2113 / 6800H / P1111 / IT5571 P.11 only. The addresses and
//! linked-output protocol were verified by the user's RedmiFanControl
//! project (MIT; see docs/fan-control.md). Unknown firmware is never written.

use std::collections::BTreeSet;
use std::io::{BufRead, Write};
use std::os::windows::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use serde::{Deserialize, Serialize};
use windows::core::w;
use crate::pawnio::Module;
use crate::reading::{BoardSensors, FanCommand, FanControl};
use crate::sensors::NamedLock;
use crate::fan_settings::FanRules;

const ISA: windows::core::PCWSTR = w!("Global\\Access_ISABUS.HTP.Method");
const OWNER: windows::core::PCWSTR = w!("Global\\RedmiFanControl.TM2113.ControlOwner");
const FLAGS: u16 = 0x0100;
const TARGETS: [u16; 2] = [0x0105, 0x0106];
const MANUAL: u8 = 8;
const EVERY: Duration = Duration::from_millis(500);

fn matches_machine(model: &str, board: &str, bios: &str, cpu: &str) -> bool {
    model == "Redmi Book Pro 15 2022" && board == "TM2113" && bios == "RMARB5B0P1111" && cpu.split_whitespace().any(|word| word == "6800H")
}

struct Hardware { lpc: Module, amd: Module }

impl Hardware {
    fn open() -> Option<Self> {
        use crate::metrics::reg_string;
        let bios = w!(r"HARDWARE\DESCRIPTION\System\BIOS");
        if !matches_machine(&reg_string(bios, w!("SystemProductName")), &reg_string(bios, w!("BaseBoardProduct")),
            &reg_string(bios, w!("BIOSVersion")), &reg_string(w!(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0"), w!("ProcessorNameString"))) { return None; }
        let hw = Self { lpc: Module::load(include_bytes!("../pawnio-modules/LpcIO.bin")).ok()?, amd: Module::load(include_bytes!("../pawnio-modules/AMDFamily17.bin")).ok()? };
        hw.session().ok()?.snapshot().ok()?;
        Some(hw)
    }

    fn temperature(&self) -> Result<f32, ()> {
        let _bus = NamedLock::acquire(w!("Global\\Access_PCI"), 1000).ok_or(())?;
        let mut out = [0];
        if self.amd.call("ioctl_read_smn", &[0x59800], &mut out).map_err(|_| ())? != 1 { return Err(()); }
        let raw = u32::try_from(out[0]).map_err(|_| ())?;
        if raw == 0 || raw == u32::MAX { return Err(()); }
        let t = (raw >> 21) as f32 * 0.125 - if raw & 0x80000 != 0 { 49.0 } else { 0.0 };
        (5.0..=115.0).contains(&t).then_some(t).ok_or(())
    }

    fn session(&self) -> Result<Session<'_>, ()> {
        let bus = NamedLock::acquire(ISA, 1000).ok_or(())?;
        self.lpc.call("ioctl_select_slot", &[1], &mut []).map_err(|_| ())?;
        let mut session = Session { hw: self, saved: None, _bus: bus };
        for key in [0x87, 0x01, 0x55, 0xaa] { session.out(0x4e, key)?; }
        for _ in 0..2 { if session.read(0x20)? != 0x55 || session.read(0x21)? != 0x71 { return Err(()); } }
        session.saved = Some([session.read(0x2e)?, session.depth(0x11)?, session.depth(0x10)?]);
        if [session.get(0x400)?, session.get(0x401)?, session.get(0x402)?] != [0x50, 1, 0x11]
            || session.get(0x2000)? != 0x55 || session.get(0x2001)? != 0x71 || session.get(0x200d)? & 1 == 0 { return Err(()); }
        Ok(session)
    }
}

struct Session<'a> { hw: &'a Hardware, saved: Option<[u8; 3]>, _bus: NamedLock }

impl Session<'_> {
    fn out(&self, port: u64, value: u8) -> Result<(), ()> { self.hw.lpc.call("ioctl_pio_outb", &[port, value as u64], &mut []).map(|_| ()).map_err(|_| ()) }
    fn sio_out(&self, index: u8, value: u8) -> Result<(), ()> { self.hw.lpc.call("ioctl_superio_outb", &[index as u64, value as u64], &mut []).map(|_| ()).map_err(|_| ()) }
    fn read(&self, index: u8) -> Result<u8, ()> {
        let mut out = [0];
        if self.hw.lpc.call("ioctl_superio_inb", &[index as u64], &mut out).map_err(|_| ())? != 1 { return Err(()); }
        u8::try_from(out[0]).map_err(|_| ())
    }
    fn depth(&self, index: u8) -> Result<u8, ()> { self.sio_out(0x2e, index)?; self.read(0x2f) }
    fn latch(&self, index: u8, value: u8) -> Result<(), ()> { self.sio_out(0x2e, index)?; self.sio_out(0x2f, value) }
    fn address(&self, address: u16) -> Result<(), ()> {
        self.latch(0x11, (address >> 8) as u8)?;
        self.latch(0x10, address as u8)?;
        if self.depth(0x11)? != (address >> 8) as u8 || self.depth(0x10)? != address as u8 { return Err(()); }
        Ok(())
    }
    fn get(&self, address: u16) -> Result<u8, ()> {
        if !matches!(address, FLAGS | 0x105 | 0x106 | 0x298 | 0x299 | 0x2b8 | 0x2b9 | 0x400..=0x402 | 0x2000 | 0x2001 | 0x200d | 0x1804 | 0x1805 | 0x1841 | 0x185b..=0x185d | 0x180a | 0x180f) { return Err(()); }
        self.address(address)?; self.depth(0x12)
    }
    fn put(&self, address: u16, value: u8) -> Result<(), ()> {
        if !matches!(address, FLAGS | 0x105 | 0x106) { return Err(()); }
        if self.get(0x200d)? & 3 != 3 { return Err(()); }
        self.address(address)?; self.sio_out(0x2e, 0x12)?; self.sio_out(0x2f, value)?;
        if self.get(address)? != value { return Err(()); }
        Ok(())
    }
    fn word(&self, address: u16) -> Result<u16, ()> {
        for _ in 0..3 {
            let (high, low) = (self.get(address)?, self.get(address + 1)?);
            if self.get(address)? == high {
                let rpm = (high as u16) << 8 | low as u16;
                return (rpm <= 10_000).then_some(rpm).ok_or(());
            }
        }
        Err(())
    }
    fn control_state(&self) -> Result<State, ()> {
        let targets = [self.get(0x105)?, self.get(0x106)?];
        let valid = self.get(0x200d).is_ok_and(|v| v & 3 == 3) && self.get(0x1841) == Ok(127) && [0x185b, 0x185c, 0x185d, 0x180a, 0x180f].into_iter().all(|r| self.get(r) == Ok(0));
        Ok(State { flags: self.get(FLAGS)?, targets, pwm: [0, 0], rpm: [0, 0], valid })
    }
    fn snapshot(&self) -> Result<State, ()> {
        let mut state = self.control_state()?;
        state.pwm = [self.get(0x1804)?, self.get(0x1805)?];
        if state.targets.iter().chain(&state.pwm).any(|v| *v > 127) { return Err(()); }
        state.rpm = [self.word(0x298)?, self.word(0x2b8)?];
        Ok(state)
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        if let Some([selector, high, low]) = self.saved { let _ = self.latch(0x11, high); let _ = self.latch(0x10, low); let _ = self.sio_out(0x2e, selector); }
        let _ = self.out(0x4e, 2); let _ = self.out(0x4f, 2);
    }
}

#[derive(Clone, Debug, Serialize)]
struct State { flags: u8, targets: [u8; 2], pwm: [u8; 2], rpm: [u16; 2], valid: bool }
impl State { fn manual(&self) -> bool { self.flags & MANUAL != 0 } }

fn raw(percent: u8) -> Result<u8, ()> {
    if percent != 0 && !(20..=100).contains(&percent) { return Err(()); }
    Ok(if percent == 100 { 127 } else { ((percent as u32 * 128 + 50) / 100).min(126) as u8 })
}
fn percent(value: u8) -> u8 { if value == 127 { 100 } else { ((value as u32 * 100 + 64) / 128) as u8 } }
fn belongs(state: &State, allowed: &[u8]) -> bool { !state.manual() || state.targets.iter().all(|v| allowed.contains(v)) }
#[cfg(test)]
fn output(requested: u8, temperature: f32) -> Result<u8, ()> {
    output_with(FanRules::default(), requested, temperature)
}
fn output_with(rules: FanRules, requested: u8, temperature: f32) -> Result<u8, ()> {
    raw(requested)?;
    if !rules.valid() || !temperature.is_finite() || !(5.0..rules.restore_at as f32).contains(&temperature) { return Err(()); }
    raw(requested.max(rules.minimum(temperature)))
}

#[derive(Default)]
struct Policy { prepared: bool, starting: Option<Instant>, stalled: Option<Instant> }
impl Policy {
    fn next(&mut self, rules: FanRules, requested: u8, t: f32, rpm: [u16; 2], now: Instant) -> Result<u8, ()> {
        let target = output_with(rules, requested, t)?;
        if target == 0 { *self = Self::default(); return Ok(0); }
        if rpm.iter().all(|rpm| *rpm > 0) { self.prepared = true; self.starting = None; self.stalled = None; return Ok(target); }
        if !self.prepared { self.prepared = true; self.starting = Some(now); }
        let limit = if self.starting.is_some() { 8 } else { 5 };
        if now.duration_since(*self.stalled.get_or_insert(now)) >= Duration::from_secs(limit) { return Err(()); }
        let boost = self.starting.map_or(0, |start| match now.duration_since(start).as_secs() { 0 => 0, 1 => 30, 2 => 40, 3 => 60, 4 => 80, _ => 100 });
        Ok(target.max(raw(boost)?))
    }
}

#[derive(Default)]
struct Protection { fault: bool, stalled: Option<Instant>, unsafe_since: Option<Instant> }
impl Protection {
    #[cfg(test)]
    fn trips(&mut self, now: Instant, since: Instant, disconnected: bool, temperature: Result<f32, ()>, state: &Result<State, ()>) -> bool {
        self.trips_with(FanRules::default(), now, since, disconnected, temperature, state)
    }
    fn trips_with(&mut self, rules: FanRules, now: Instant, since: Instant, disconnected: bool, temperature: Result<f32, ()>, state: &Result<State, ()>) -> bool {
        let stopped = state.as_ref().is_ok_and(|s| s.targets.iter().all(|v| *v > 0) && s.rpm.contains(&0));
        let stall = if stopped { now.duration_since(*self.stalled.get_or_insert(now)) >= Duration::from_secs(12) }
            else { self.stalled = None; false };
        let too_low = temperature.ok().zip(state.as_ref().ok()).is_some_and(|(t,s)| s.targets.iter().any(|v| *v < raw(rules.minimum(t)).unwrap_or(127)));
        let unsafe_output = if too_low { now.duration_since(*self.unsafe_since.get_or_insert(now)) >= Duration::from_secs(3) } else { self.unsafe_since = None; false };
        self.fault |= disconnected || now.duration_since(since) >= Duration::from_secs(4) || temperature.is_err()
            || !rules.valid() || temperature.is_ok_and(|t| !t.is_finite() || !(5.0..rules.restore_at as f32).contains(&t)) || state.is_err() || stall || unsafe_output;
        self.fault
    }
}

trait ControlIo { fn state(&self) -> Result<State, ()>; fn write(&self, address: u16, value: u8) -> Result<(), ()>; }
impl ControlIo for Session<'_> {
    fn state(&self) -> Result<State, ()> { self.control_state() }
    fn write(&self, address: u16, value: u8) -> Result<(), ()> { self.put(address, value) }
}

fn set(io: &impl ControlIo, target: u8, previous: Option<u8>) -> Result<State, ()> {
    let before = io.state()?;
    if !before.valid || (before.manual() && !previous.is_some_and(|v| before.targets == [v, v])) { return Err(()); }
    for address in TARGETS { io.write(address, target)?; }
    let flags = io.state()?.flags;
    if flags != before.flags { return Err(()); }
    io.write(FLAGS, flags | MANUAL)?;
    let after = io.state()?;
    (after.manual() && after.targets == [target, target]).then_some(after).ok_or(())
}

fn restore(io: &impl ControlIo, allowed: &[u8]) -> Result<(), ()> {
    let state = io.state()?;
    if !belongs(&state, allowed) { return Err(()); }
    io.write(FLAGS, state.flags & !MANUAL)?;
    for address in TARGETS { io.write(address, 0)?; }
    let after = io.state()?;
    (!after.manual() && after.targets == [0, 0]).then_some(()).ok_or(())
}

/// A separate process owns the recovery ticket before a manual write.
/// Its stdin closes when the worker dies; a stalled worker stops renewing
/// its lease. It samples CPU temperature and RPM independently.
#[derive(Clone, Serialize, Deserialize)]
struct Ticket { sequence: u64, allowed: Vec<u8>, armed: bool, rules: FanRules }

struct Guard { child: Child, input: ChildStdin, replies: mpsc::Receiver<u64>, sequence: u64 }

impl Guard {
    fn start() -> Result<Self, ()> {
        let current = std::env::current_exe().map_err(|_| ())?;
        let program = current.parent().ok_or(())?.join("glance-sensors.exe");
        let mut child = Command::new(program).arg("--redmi-fan-guardian")
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).creation_flags(0x08000000).spawn().map_err(|_| ())?;
        let input = child.stdin.take().ok_or(())?;
        let output = child.stdout.take().ok_or(())?;
        let (reply, replies) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(output).lines().map_while(Result::ok) {
                if let Ok(id) = line.parse() { if reply.send(id).is_err() { break; } }
            }
        });
        let mut guard = Self { child, input, replies, sequence: 0 };
        guard.tell(&[], false, FanRules::default())?;
        Ok(guard)
    }
    fn tell(&mut self, allowed: &[u8], armed: bool, rules: FanRules) -> Result<(), ()> {
        self.sequence += 1;
        let ticket = Ticket { sequence: self.sequence, allowed: allowed.to_vec(), armed, rules };
        serde_json::to_writer(&mut self.input, &ticket).map_err(|_| ())?;
        self.input.write_all(b"\n").and_then(|_| self.input.flush()).map_err(|_| ())?;
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            let id = self.replies.recv_timeout(until.saturating_duration_since(Instant::now())).map_err(|_| ())?;
            if id == self.sequence { return Ok(()); }
        }
    }
}

impl Drop for Guard { fn drop(&mut self) { /* Never kill the recovery process. Closing stdin makes it restore. */ let _ = self.child.try_wait(); } }

pub fn guardian() {
    // The guardian holds the existing project's ownership mutex, so it
    // remains exclusive even after its worker crashes and while restoring.
    let Some(_owner) = NamedLock::acquire(OWNER, 0) else { return };
    let Some(hw) = Hardware::open() else { return };
    let (sent, received) = mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if line.len() > 1024 { break; }
            let Ok(ticket) = serde_json::from_str::<Ticket>(&line) else { break };
            if !ticket.rules.valid() || ticket.allowed.len() > 3 || ticket.allowed.iter().any(|v| *v > 127) || (ticket.armed && ticket.allowed.is_empty()) { break; }
            if sent.send(ticket).is_err() { break; }
        }
    });
    let mut ticket = Ticket { sequence: 0, allowed: Vec::new(), armed: false, rules: FanRules::default() };
    let mut since = Instant::now();
    let mut protection = Protection::default();
    loop {
        let mut disconnected = false;
        match received.recv_timeout(Duration::from_millis(350)) {
            Ok(next) if next.sequence > ticket.sequence => {
                if !next.armed || !protection.fault {
                    ticket = next; since = Instant::now();
                    // Acknowledge only after the ticket is held by this process.
                    let mut output = std::io::stdout().lock();
                    if writeln!(output, "{}", ticket.sequence).and_then(|_| output.flush()).is_err() { disconnected = true; }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => disconnected = true,
            _ => {}
        }
        if ticket.armed {
            let t = hw.temperature();
            let state = hw.session().and_then(|s| s.snapshot());
            if protection.trips_with(ticket.rules, Instant::now(), since, disconnected, t, &state) {
                // Retain the ticket and retry if the bus is busy. Never clear
                // a manual target that no longer belongs to this worker.
                if let Ok(session) = hw.session() {
                    if let Ok(state) = session.control_state() {
                        if !belongs(&state, &ticket.allowed) || (restore(&session, &ticket.allowed).is_ok() && disconnected) { ticket.armed = false; }
                    }
                }
            }
        }
        if disconnected && !ticket.armed { break; }
        if disconnected { std::thread::sleep(Duration::from_millis(350)); }
    }
}

struct Worker {
    hw: Hardware, guard: Option<Guard>,
    owned: Option<u8>, requested: u8, recovering: bool, failed: bool, rules: FanRules, policy: Policy,
    allowed: BTreeSet<u8>,
}

impl Worker {
    fn new(hw: Hardware) -> Self { Self { hw, guard: None, owned: None, requested: 40, recovering: false, failed: false, rules: FanRules::default(), policy: Policy::default(), allowed: BTreeSet::new() } }
    fn recover(&mut self) -> bool {
        if self.owned.is_none() && self.allowed.is_empty() { return true; }
        self.recovering = true;
        let allowed: Vec<_> = self.allowed.iter().copied().collect();
        let okay = self.hw.session().and_then(|s| restore(&s, &allowed)).is_ok();
        if okay {
            if let Some(guard) = &mut self.guard { let _ = guard.tell(&[], false, self.rules); }
            self.guard = None; self.owned = None; self.allowed.clear(); self.recovering = false; self.policy = Policy::default();
        }
        okay
    }
    fn command(&mut self, command: FanCommand, until: Instant) -> bool {
        match command {
            FanCommand::Rules(rules) => { if !rules.valid() { return false; } self.rules = rules; return true; }
            FanCommand::Automatic(0) | FanCommand::RestoreAll => return self.recover(),
            FanCommand::Fixed { channel: 0, percent: requested } if raw(requested).is_ok() => {
                let Ok(t) = self.hw.temperature() else { self.failed = true; self.recover(); return false };
                let state = self.hw.session().and_then(|s| s.snapshot());
                let target = state.and_then(|s| self.policy.next(self.rules, requested, t, s.rpm, Instant::now()));
                let Ok(target) = target else { self.failed = true; self.recover(); return false };
                if self.recovering || !self.apply_target(target, until) { return false; }
                self.requested = requested; return true;
            }
            _ => return false,
        }
    }
    fn apply_target(&mut self, target: u8, until: Instant) -> bool {
        if self.guard.is_none() { self.guard = Guard::start().ok(); }
        let Some(guard) = &mut self.guard else { self.policy = Policy::default(); return false };
        self.allowed.insert(target);
        if let Some(old) = self.owned { self.allowed.insert(old); }
        let allowed: Vec<_> = self.allowed.iter().copied().collect();
        if guard.tell(&allowed, true, self.rules).is_err() { self.failed = true; self.recover(); return false; }
        let result = self.hw.session().and_then(|s| {
            if Instant::now() >= until { return Err(()); }
            set(&s, target, self.owned)
        });
        if result.is_err() { self.failed = true; self.recover(); return false; }
        self.owned = Some(target); self.allowed = BTreeSet::from([target]); self.failed = false; true
    }
    fn read(&mut self) -> BoardSensors {
        let temperature = self.hw.temperature().ok();
        let mut state = self.hw.session().and_then(|s| s.snapshot());
        let mut changed = false;
        if let Ok(state) = &state {
            let controlling = self.owned.is_some() || !self.allowed.is_empty();
            let external = state.manual() && !state.targets.iter().all(|v| self.allowed.contains(v));
            if controlling && (external || (!state.manual() && !self.recovering && self.owned.is_some())) {
                // Firmware or another controller took over. Disarm without writing.
                if let Some(guard) = &mut self.guard { let _ = guard.tell(&[], false, self.rules); }
                self.guard = None; self.owned = None; self.allowed.clear(); self.failed = true; self.policy = Policy::default();
            }
        }
        if self.owned.is_some() || !self.allowed.is_empty() {
            let next = temperature.ok_or(()).and_then(|t| state.as_ref().map_err(|_| ()).and_then(|s| self.policy.next(self.rules, self.requested, t, s.rpm, Instant::now())));
            if self.recovering || next.is_err() {
                self.failed = true; self.recover(); changed = true;
            } else {
                // Renew the independent lease even if no output change is needed.
                let next = next.unwrap();
                if self.owned != Some(next) { self.apply_target(next, Instant::now() + Duration::from_secs(3)); changed = true; }
                else if self.guard.as_mut().is_none_or(|g| g.tell(&self.allowed.iter().copied().collect::<Vec<_>>(), true, self.rules).is_err()) { self.failed = true; self.recover(); changed = true; }
            }
        }
        // A command above may have restored or changed targets; publish real readback.
        if changed { state = self.hw.session().and_then(|s| s.snapshot()); }
        let mut board = BoardSensors { fan_control_failed: self.failed, ..Default::default() };
        let Ok(state) = state else { return board };
        if let Some(t) = temperature { board.temps.push(("cpu".into(), t)); }
        let free = self.guard.is_some() || NamedLock::acquire(OWNER, 0).is_some();
        board.fan_control_busy = !free;
        for (i, name) in ["redmi_fan_1", "redmi_fan_2"].into_iter().enumerate() {
            board.fans.push((name.into(), state.rpm[i] as f32));
            board.fan_controls.push(FanControl { channel: 0, name: name.into(), percent: self.owned.map(percent), manual: self.owned.is_some(),
                available: free && state.valid && temperature.is_some_and(|t| t < self.rules.restore_at as f32) && (!state.manual() || self.owned.is_some()), linked: true,
                can_stop: true, requested_percent: self.owned.map(|_| self.requested) });
        }
        board
    }
}

pub struct Watched { latest: Arc<Mutex<BoardSensors>>, stop: Arc<AtomicBool> }
impl Watched {
    pub fn open() -> Option<Self> {
        let hw = Hardware::open()?;
        let (latest, stop) = (Arc::new(Mutex::new(BoardSensors::default())), Arc::new(AtomicBool::new(false)));
        let (kept, stopped) = (latest.clone(), stop.clone());
        let (sent, received) = mpsc::channel::<crate::fan_control::Task>();
        crate::fan_control::register(sent);
        std::thread::spawn(move || {
            let mut worker = Worker::new(hw);
            while !stopped.load(Ordering::Relaxed) {
                let began = Instant::now();
                *kept.lock().unwrap() = worker.read();
                let every = if worker.owned.is_some() || !worker.allowed.is_empty() { EVERY } else { Duration::from_secs(1) };
                while !stopped.load(Ordering::Relaxed) {
                    match received.recv_timeout(every.saturating_sub(began.elapsed())) {
                        Ok(task) => {
                            let restore = matches!(task.command, FanCommand::RestoreAll | FanCommand::Automatic(_));
                            let okay = (restore || Instant::now() < task.until) && worker.command(task.command, task.until);
                            let _ = task.reply.send(okay);
                            if began.elapsed() >= every { break; }
                        }
                        Err(_) => break,
                    }
                }
            }
            worker.recover(); // The separate guardian retries if the bus is still busy.
        });
        Some(Self { latest, stop })
    }
    pub fn read(&self) -> BoardSensors { self.latest.lock().unwrap().clone() }
}
impl Drop for Watched { fn drop(&mut self) { self.stop.store(true, Ordering::Relaxed); } }

/// Explicit read-only diagnostic for hardware verification; never changes fan control.
#[cfg(feature = "studio")]
pub fn diagnose(path: &std::path::Path) {
    let result = Hardware::open().map(|hw| {
        let temperature = hw.temperature().ok();
        let state = hw.session().and_then(|s| s.snapshot()).ok();
        let board = Worker::new(hw).read();
        serde_json::json!({"temperature":temperature,"state":state,"board":board})
    });
    if let Ok(json) = serde_json::to_vec_pretty(&result) { let _ = std::fs::write(path, json); }
}

/// Development-only, explicit bounded hardware check. Never called by CI,
/// normal startup or the offline tests. A busy owner or CPU >=70°C aborts.
#[cfg(feature = "studio")]
pub fn verify_control(path: &std::path::Path, crash: bool) {
    let result = (|| {
        let hw = Hardware::open().ok_or("hardware unavailable")?;
        let before = hw.session().and_then(|s| s.snapshot()).map_err(|_| "no initial reading")?;
        let t = hw.temperature().map_err(|_| "no temperature")?;
        if t >= 70.0 || before.manual() { return Err("keep automatic: temperature or existing manual control"); }
        let mut worker = Worker::new(hw);
        if !worker.command(FanCommand::Fixed { channel: 0, percent: 40 }, Instant::now() + Duration::from_secs(3)) { worker.recover(); return Err("control not confirmed (possibly another controller)"); }
        let manual = worker.hw.session().and_then(|s| s.snapshot()).ok();
        let confirmed = manual.as_ref().is_some_and(|s| s.manual() && s.targets == [51, 51] && s.rpm.iter().all(|rpm| *rpm > 0));
        if crash && confirmed {
            let report = serde_json::json!({"phase":"manual confirmed; worker exits without cleanup", "before":before, "manual":manual});
            let _ = std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap());
            std::process::exit(0); // The independent process must restore on stdin EOF.
        }
        for _ in 0..6 {
            std::thread::sleep(Duration::from_millis(500));
            if !worker.hw.temperature().is_ok_and(|t| t < 75.0) { break; }
            worker.read();
            if worker.failed || worker.owned.is_none() { break; }
        }
        let restored = worker.recover();
        let after = worker.hw.session().and_then(|s| s.snapshot()).ok();
        Ok(serde_json::json!({"before":before,"manual":manual,"manual_confirmed":confirmed,"restored":restored,"after":after}))
    })();
    let report = match result { Ok(report) => report, Err(why) => serde_json::json!({"aborted":why}) };
    let _ = std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap());
}

/// Explicit low-output check: at most a few seconds per preset, automatic
/// restoration after each. Actual stop is tested only below 45°C.
#[cfg(feature = "studio")]
pub fn verify_low(path: &std::path::Path) {
    let result = (|| {
        let hw = Hardware::open().ok_or("hardware unavailable")?;
        let mut worker = Worker::new(hw);
        let mut checks = Vec::new();
        for requested in [20,30,0] {
            let t = worker.hw.temperature().map_err(|_| "no temperature")?;
            let before = worker.hw.session().and_then(|s| s.snapshot()).map_err(|_| "no reading")?;
            if t >= 70.0 || before.manual() { return Err("keep automatic: temperature or existing manual control"); }
            if requested == 0 && t >= 45.0 { checks.push(serde_json::json!({"requested":0,"skipped":"actual stop requires CPU below 45 C in this diagnostic","temperature":t})); continue; }
            if !worker.command(FanCommand::Fixed { channel: 0, percent: requested }, Instant::now() + Duration::from_secs(3)) { worker.recover(); return Err("control not confirmed"); }
            let mut readings = Vec::new();
            for _ in 0..(if requested == 0 { 12 } else { 4 }) {
                std::thread::sleep(Duration::from_millis(500));
                let t = worker.hw.temperature().ok();
                if t.is_none_or(|t| t >= if requested == 0 { 48.0 } else { 75.0 }) { break; }
                let board = worker.read(); readings.push(serde_json::json!({"temperature":t,"fans":board.fans,"controls":board.fan_controls}));
                if worker.failed || worker.owned.is_none() { break; }
            }
            let restored = worker.recover();
            let after = worker.hw.session().and_then(|s| s.snapshot()).ok();
            checks.push(serde_json::json!({"requested":requested,"readings":readings,"restored":restored,"after":after}));
            if !restored { return Err("restore not confirmed; guardian retains recovery ticket"); }
        }
        Ok(serde_json::json!({"checks":checks}))
    })();
    let report = match result { Ok(report) => report, Err(why) => serde_json::json!({"aborted":why}) };
    let _ = std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    struct Fake { state: RefCell<State>, writes: RefCell<Vec<u16>>, fail: Option<u16> }
    impl ControlIo for Fake {
        fn state(&self) -> Result<State, ()> { Ok(self.state.borrow().clone()) }
        fn write(&self, address: u16, value: u8) -> Result<(), ()> {
            if self.fail == Some(address) { return Err(()); }
            self.writes.borrow_mut().push(address);
            let mut s = self.state.borrow_mut();
            match address { FLAGS => s.flags = value, 0x105 => s.targets[0] = value, 0x106 => s.targets[1] = value, _ => panic!("outside whitelist") }
            Ok(())
        }
    }
    fn fake() -> Fake { Fake { state: RefCell::new(State { flags: 0xa0, targets: [0, 0], pwm: [40, 40], rpm: [2800, 2850], valid: true }), writes: RefCell::new(Vec::new()), fail: None } }
    #[test]
    fn only_the_validated_machine_matches() {
        assert!(matches_machine("Redmi Book Pro 15 2022", "TM2113", "RMARB5B0P1111", "AMD Ryzen 7 6800H"));
        for (model, board, bios, cpu) in [("Redmi Book Pro 15 2023", "TM2113", "RMARB5B0P1111", "6800H"), ("Redmi Book Pro 15 2022", "other", "RMARB5B0P1111", "6800H"), ("Redmi Book Pro 15 2022", "TM2113", "RMARB5B0P1112", "6800H"), ("Redmi Book Pro 15 2022", "TM2113", "RMARB5B0P1111", "6600H")] { assert!(!matches_machine(model, board, bios, cpu)); }
    }
    #[test]
    fn linked_targets_are_staged_before_manual_and_flags_are_preserved() {
        let io = fake(); let target = raw(40).unwrap();
        set(&io, target, None).unwrap();
        assert_eq!(*io.writes.borrow(), [0x105, 0x106, FLAGS]);
        assert_eq!(io.state.borrow().flags, 0xa8);
        io.state.borrow_mut().flags |= 1;
        restore(&io, &[target]).unwrap();
        assert_eq!((io.state.borrow().flags, io.state.borrow().targets), (0xa1, [0, 0]));
        assert_eq!((raw(40), raw(60), raw(80), raw(100)), (Ok(51), Ok(77), Ok(102), Ok(127)));
        assert_eq!((raw(0), raw(20), raw(30)), (Ok(0), Ok(26), Ok(38)));
        assert!(raw(10).is_err() && raw(101).is_err());
    }
    #[test]
    fn a_partial_write_is_recoverable_but_external_control_is_preserved() {
        let mut io = fake(); io.fail = Some(0x106);
        assert!(set(&io, 51, None).is_err());
        assert!(!io.state.borrow().manual());
        io.fail = None; restore(&io, &[51]).unwrap();
        set(&io, 51, None).unwrap();
        io.state.borrow_mut().targets = [80, 80];
        assert!(set(&io, 77, Some(51)).is_err());
        assert!(restore(&io, &[51, 77]).is_err());
        assert_eq!(io.state.borrow().targets, [80, 80]);
    }
    #[test]
    fn guardian_latches_temperature_sensor_lease_and_stall_failures() {
        let now = Instant::now(); let state = fake().state();
        assert!(!Protection::default().trips(now, now, false, Ok(45.0), &state));
        for temperature in [Err(()), Ok(95.0), Ok(f32::NAN)] {
            let mut protection = Protection::default();
            assert!(protection.trips(now, now, false, temperature, &state));
            assert!(protection.trips(now, now, false, Ok(45.0), &state)); // A new ticket must not silently re-arm it.
        }
        assert!(Protection::default().trips(now, now, true, Ok(45.0), &state));
        assert!(Protection::default().trips(now + Duration::from_secs(4), now, false, Ok(45.0), &state));
        assert!(Protection::default().trips(now, now, false, Ok(45.0), &Err(())));
        let mut stopped = state.unwrap(); stopped.rpm[1] = 0; stopped.targets = [26,26];
        let mut protection = Protection::default();
        assert!(!protection.trips(now, now, false, Ok(45.0), &Ok(stopped.clone())));
        assert!(protection.trips(now + Duration::from_secs(12), now + Duration::from_secs(12), false, Ok(45.0), &Ok(stopped)));
        assert_eq!((output(40, 79.0), output(40, 80.0), output(40, 95.0)), (Ok(51), Ok(127), Err(())));
    }
    #[test]
    fn intentional_stop_is_not_a_stall_and_warming_restarts_both_fans() {
        let r = FanRules::default(); let now = Instant::now(); let mut p = Policy::default();
        for seconds in [0,5,60,600] { assert_eq!(p.next(r, 0, 49.0, [0,0], now + Duration::from_secs(seconds)), Ok(0)); }
        assert_eq!(p.next(r, 0, 50.0, [0,0], now + Duration::from_secs(601)), Ok(26));
        assert_eq!(p.next(r, 0, 60.0, [1200,1200], now + Duration::from_secs(602)), Ok(38));
        assert_eq!(p.next(r, 20, 80.0, [1200,1200], now + Duration::from_secs(603)), Ok(127));
        assert!(p.next(r, 0, 95.0, [1200,1200], now).is_err());
        let state = State { flags: 8, targets: [0,0], pwm: [0,0], rpm: [0,0], valid: true };
        let mut guard = Protection::default();
        assert!(!guard.trips(now, now, false, Ok(49.0), &Ok(state.clone())));
        assert!(!guard.trips(now + Duration::from_secs(60), now + Duration::from_secs(60), false, Ok(49.0), &Ok(state.clone())));
        assert!(!guard.trips(now + Duration::from_secs(61), now + Duration::from_secs(61), false, Ok(50.0), &Ok(state.clone())));
        assert!(guard.trips(now + Duration::from_secs(64), now + Duration::from_secs(64), false, Ok(50.0), &Ok(state)));
    }
    #[test]
    fn low_speed_startup_tries_the_target_then_boosts_only_until_both_turn() {
        let r = FanRules::default(); let now = Instant::now(); let mut p = Policy::default();
        for (seconds, expected) in [26,38,51,77,102,127].into_iter().enumerate() {
            assert_eq!(p.next(r, 20, 45.0, [0,0], now + Duration::from_secs(seconds as u64)), Ok(expected));
        }
        assert_eq!(p.next(r, 20, 45.0, [1200,1300], now + Duration::from_secs(6)), Ok(26));
        assert_eq!(p.next(r, 20, 45.0, [0,1300], now + Duration::from_secs(7)), Ok(26));
        assert!(p.next(r, 20, 45.0, [0,1300], now + Duration::from_secs(12)).is_err());
        let mut failed = Policy::default();
        assert!(failed.next(r, 30, 45.0, [0,0], now).is_ok());
        assert!(failed.next(r, 30, 45.0, [0,0], now + Duration::from_secs(8)).is_err());
    }
    #[test]
    fn worker_and_guardian_use_the_same_custom_thresholds() {
        let rules = FanRules { stop_below: 40, boost_at: 65, full_at: 75, restore_at: 90 };
        assert_eq!((output_with(rules,0,39.0),output_with(rules,0,40.0),output_with(rules,20,65.0),output_with(rules,20,75.0),output_with(rules,20,90.0)), (Ok(0),Ok(26),Ok(38),Ok(127),Err(())));
        let now = Instant::now(); let state = fake().state();
        assert!(Protection::default().trips_with(rules, now, now, false, Ok(90.0), &state));
    }
}
