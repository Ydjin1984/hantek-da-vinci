//! Настройки, которые должны пережить перезапуск программы.
//!
//! Хранятся рядом с калибровкой, в `%LOCALAPPDATA%\DaVinci\settings.json`.
//! Файл читается один раз при старте и перезаписывается при каждом изменении:
//! он крошечный, а «последнее состояние» должно быть всегда, даже если
//! программу закрыли крестиком, не нажав «Стоп».

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
    /// Развёртка, индекс 0..25.
    #[serde(default = "default_time_div_id")]
    pub time_div_id: u8,
    /// Авто-масштаб по каналам.
    #[serde(default)]
    pub autoscale: Vec<bool>,
}

fn default_time_div_id() -> u8 {
    DEFAULT_TIME_DIV_ID
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: (0..CHANNELS).collect(),
            ranges: vec![DEFAULT_RANGE_ID; CHANNELS],
            time_div_id: DEFAULT_TIME_DIV_ID,
            autoscale: vec![false; CHANNELS],
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
        if let Ok(text) = serde_json::to_string_pretty(&clean) {
            let _ = std::fs::write(Self::path(), text);
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
    }

    /// Пределы в виде, который ждёт прибор.
    pub fn range_array(&self) -> [Range; CHANNELS] {
        let mut out = [Range::V20; CHANNELS];
        for (i, slot) in out.iter_mut().enumerate() {
            let id = self.ranges.get(i).copied().unwrap_or(DEFAULT_RANGE_ID);
            *slot = Range::from_id(id).unwrap_or(Range::V20);
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
