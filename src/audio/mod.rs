//! 音频层：WASAPI 设备与会话管理，与控制台界面完全解耦。

pub mod appdevice;
pub mod appinfo;
pub mod com;
pub mod engine;
pub mod icons;
pub mod model;
pub mod policy;

pub use engine::{AudioCommand, AudioEngine, AudioEvent};
pub use model::{
    AppInfo, DeviceInfo, DeviceKind, LOG_MIN_DB, Snapshot, amplitude_to_db, db_to_amplitude,
    display_to_db, percent_from_display, readings, to_display_volume, to_log_volume, volume_label,
    volume_to_percent,
};
