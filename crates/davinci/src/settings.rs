//! Настройки, которые должны пережить перезапуск программы.
//!
//! Хранятся рядом с калибровкой, в `%LOCALAPPDATA%\DaVinci\settings.json`.
//! Файл читается один раз при старте и перезаписывается при каждом изменении:
//! он крошечный, а «последнее состояние» должно быть всегда, даже если
//! программу закрыли крестиком, не нажав «Стоп».

use crate::trigger::Config as TriggerConfig;
use hantek1008::{Device, Range, TimeDiv, CHANNELS};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Значения по умолчанию: все каналы, ±20 В, 500 µs/дел, авто-масштаб выключен.
const DEFAULT_RANGE_ID: u8 = 3;
const DEFAULT_TIME_DIV_ID: u8 = 17;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Включённые каналы, индексы 0..8.
    #[serde(default)]
    pub enabled: Vec<usize>,
    /// Предел каждого канала, id 1..3.
    #[serde(default)]
    pub ranges: Vec<u8>,
    /// Полная шкала, выбранная пользователем, в вольтах (±). Тоньше ±0,41 В —
    /// цифровое сужение внутри младшего предела прибора.
    #[serde(default)]
    pub range_v: Vec<f32>,
    /// Развёртка, индекс 0..25.
    #[serde(default = "default_time_div_id")]
    pub time_div_id: u8,
    /// Авто-масштаб по каналам.
    #[serde(default)]
    pub autoscale: Vec<bool>,
    /// Смещение вида по вертикали, вольты (не влияет на измерение).
    #[serde(default)]
    pub center_v: Vec<f32>,
    /// Триггер: режим, источник, фронт, уровень и положение T₀.
    ///
    /// `#[serde(default)]` на уровне поля, а не «всё или ничего»: файл от
    /// предыдущей версии не содержит триггера вообще, и он должен прочитаться,
    /// а не превратиться в настройки по умолчанию целиком.
    #[serde(default)]
    pub trigger: TriggerConfig,
}

fn default_time_div_id() -> u8 {
    DEFAULT_TIME_DIV_ID
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: (0..CHANNELS).collect(),
            ranges: vec![DEFAULT_RANGE_ID; CHANNELS],
            range_v: vec![20.0; CHANNELS],
            time_div_id: DEFAULT_TIME_DIV_ID,
            autoscale: vec![false; CHANNELS],
            center_v: vec![0.0; CHANNELS],
            trigger: TriggerConfig::default(),
        }
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        crate::app::state_dir().join("settings.json")
    }

    /// Прочитать файл, молча откатившись к умолчаниям.
    ///
    /// Битый или недописанный файл не должен мешать запуску: настройки — это
    /// удобство, а не условие работы.
    pub fn load() -> Settings {
        let raw = match std::fs::read_to_string(Self::path()) {
            Ok(t) => t,
            Err(_) => return Settings::default(),
        };
        let mut s: Settings = serde_json::from_str(&raw).unwrap_or_default();
        s.fix();
        s
    }

    /// Записать файл. Ошибка записи не фатальна и не показывается.
    pub fn save(&self) {
        let mut clean = self.clone();
        clean.fix();
        let Ok(text) = serde_json::to_string_pretty(&clean) else {
            return;
        };
        let path = Self::path();
        let tmp = path.with_extension("json.tmp");
        // Atomic replace: a power cut in the middle of a write must not leave a
        // truncated file that `load` silently turns into defaults.
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// Привести значения в допустимые границы.
    ///
    /// Файл мог быть отредактирован руками или остаться от другой версии.
    fn fix(&mut self) {
        self.enabled.retain(|&c| c < CHANNELS);
        self.enabled.sort_unstable();
        self.enabled.dedup();
        if self.enabled.is_empty() {
            self.enabled = (0..CHANNELS).collect();
        }

        self.ranges.resize(CHANNELS, DEFAULT_RANGE_ID);
        for id in &mut self.ranges {
            *id = (*id).clamp(1, 3);
        }

        if self.time_div_id > 25 {
            self.time_div_id = DEFAULT_TIME_DIV_ID;
        }

        self.autoscale.resize(CHANNELS, false);

        // Версия без `range_v` приходит со старым файлом: тогда шкала берётся
        // из сохранённого id предела.
        if self.range_v.len() < CHANNELS {
            let mut v = vec![0.0f32; CHANNELS];
            for (i, slot) in v.iter_mut().enumerate() {
                let id = self.ranges.get(i).copied().unwrap_or(DEFAULT_RANGE_ID);
                *slot = Range::from_id(id).unwrap_or(Range::V20).full_scale();
            }
            self.range_v = v;
        }
        for v in &mut self.range_v {
            *v = v.clamp(0.05, 20.0);
        }

        self.center_v.resize(CHANNELS, 0.0);
        for v in &mut self.center_v {
            if !v.is_finite() {
                *v = 0.0;
            }
            *v = v.clamp(-100.0, 100.0);
        }

        // Триггер приходит из файла (или из умолчаний) и чинится, а не
        // отбрасывается: неверный источник должен стать первым включённым
        // каналом, а не оставить программу без триггера.
        self.trigger.pretrigger_pct = if self.trigger.pretrigger_pct.is_finite() {
            self.trigger.pretrigger_pct.clamp(10.0, 90.0)
        } else {
            crate::trigger::DEFAULT_PRETRIGGER_PCT
        };
        if !self.trigger.level_v.is_finite() {
            self.trigger.level_v = 0.0;
        }
        if self.trigger.timeout_ms > crate::trigger::MAX_TIMEOUT_MS {
            self.trigger.timeout_ms = crate::trigger::DEFAULT_TIMEOUT_MS;
        }
        if self.enabled.is_empty() || !self.enabled.contains(&self.trigger.source) {
            self.trigger.source = self.enabled.first().copied().unwrap_or(0);
        }
        // Один взвод не переживает перезапуск: приложение, стартующее в
        // ожидании одного фронта, выглядит зависшим.
        if self.trigger.mode == crate::trigger::Mode::Single {
            self.trigger.mode = crate::trigger::Mode::Normal;
        }
    }

    /// Пределы в виде, который ждёт прибор: самый узкий, но покрывающий выбор.
    pub fn range_array(&self) -> [Range; CHANNELS] {
        let mut out = [Range::V20; CHANNELS];
        for (i, slot) in out.iter_mut().enumerate() {
            match self.range_v.get(i) {
                Some(v) => *slot = Range::covering(*v),
                None => {
                    let id = self.ranges.get(i).copied().unwrap_or(DEFAULT_RANGE_ID);
                    *slot = Range::from_id(id).unwrap_or(Range::V20);
                }
            }
        }
        out
    }

    /// Применить к прибору: каналы, пределы, развёртку.
    pub fn apply(&self, dev: &mut Device) {
        let _ = dev.set_channels(&self.enabled);
        let _ = dev.set_ranges(&self.range_array());
        if let Ok(td) = TimeDiv::from_id(self.time_div_id) {
            let _ = dev.set_time_div(td);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_is_never_written_to_disk() {
        // A program that starts in Single looks frozen after the first capture:
        // the saved mode must always be Normal. `persist` writes through `save`,
        // which runs `fix`, so this is the invariant that protects a restart.
        let mut s = Settings::default();
        s.trigger.mode = crate::trigger::Mode::Single;
        s.fix();
        assert_eq!(s.trigger.mode, crate::trigger::Mode::Normal);
    }

    #[test]
    fn a_broken_file_is_repaired_instead_of_rejected() {
        let mut s = Settings {
            enabled: vec![],
            ranges: vec![9, 0],
            time_div_id: 99,
            trigger: crate::trigger::Config {
                mode: crate::trigger::Mode::Single,
                source: 7,
                ..crate::trigger::Config::default()
            },
            ..Settings::default()
        };
        s.fix();
        assert_eq!(s.enabled, (0..CHANNELS).collect::<Vec<_>>());
        assert!(s.ranges.iter().all(|id| (1..=3).contains(id)));
        assert_eq!(s.time_div_id, DEFAULT_TIME_DIV_ID);
        assert!(s.enabled.contains(&s.trigger.source));
        assert_eq!(s.trigger.mode, crate::trigger::Mode::Normal);
    }
}
