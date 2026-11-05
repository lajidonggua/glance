//! Persisted menu choices and bounded thermal rules, shared by the UI,
//! worker and independent guardian. No saved setting starts manual control.
use serde::{Deserialize, Serialize};

pub const PRESETS: [u8; 10] = [0, 20, 30, 40, 50, 60, 70, 80, 90, 100];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FanSettings { pub presets: Vec<u8>, pub rules: FanRules }
impl Default for FanSettings {
    fn default() -> Self { Self { presets: vec![0, 20, 30, 40, 60, 80, 100], rules: FanRules::default() } }
}
impl FanSettings {
    pub fn presets(&self) -> Vec<u8> { PRESETS.into_iter().filter(|p| self.presets.contains(p)).collect() }
    pub fn toggle(&mut self, index: usize) {
        if let Some(&p) = PRESETS.get(index) {
            self.presets = self.presets();
            if self.presets.contains(&p) { self.presets.retain(|v| *v != p); } else { self.presets.push(p); self.presets.sort_unstable(); }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FanRules { pub stop_below: u8, pub boost_at: u8, pub full_at: u8, pub restore_at: u8 }
impl Default for FanRules {
    fn default() -> Self { Self { stop_below: 50, boost_at: 60, full_at: 80, restore_at: 95 } }
}
impl FanRules {
    pub fn valid(self) -> bool {
        (35..=55).contains(&self.stop_below) && (60..=70).contains(&self.boost_at)
            && (75..=85).contains(&self.full_at) && (90..=95).contains(&self.restore_at)
    }
    pub fn checked(self) -> Self { if self.valid() { self } else { Self::default() } }
    pub fn minimum(self, temperature: f32) -> u8 {
        if temperature >= self.full_at as f32 { 100 }
        else if temperature >= self.boost_at as f32 { 30 }
        else if temperature >= self.stop_below as f32 { 20 }
        else { 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn presets_are_multiple_choices_and_survive_save_and_reload() {
        let mut settings = FanSettings::default();
        settings.toggle(4); settings.toggle(3);
        let saved = serde_json::to_string(&settings).unwrap();
        let reloaded: FanSettings = serde_json::from_str(&saved).unwrap();
        assert_eq!(reloaded.presets(), [0,20,30,50,60,80,100]);
        assert_eq!(serde_json::from_str::<FanSettings>("{}").unwrap(), FanSettings::default());
    }
    #[test]
    fn invalid_rules_cannot_raise_the_emergency_limit_or_reverse_thresholds() {
        for rules in [FanRules { restore_at: 100, ..Default::default() }, FanRules { stop_below: 70, ..Default::default() }, FanRules { full_at: 60, ..Default::default() }] {
            assert!(!rules.valid()); assert_eq!(rules.checked(), FanRules::default());
        }
        let r = FanRules::default();
        assert_eq!([49.0,50.0,60.0,80.0].map(|t| r.minimum(t)), [0,20,30,100]);
    }
}
