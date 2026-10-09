//! The few settings that outlive a session: one `key=value` per line in
//! `%APPDATA%\DecDVB\prefs.txt` (`~/.config/DecDVB/prefs.txt` elsewhere).
//! Unknown keys are kept as they are, so an older build does not drop what a
//! newer one wrote.

use std::path::PathBuf;
use std::sync::Mutex;

/// The output folder in use: recordings, symbol files, PCAP and TS files.
static OUTPUT_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
/// The receiver's clock correction, ppm (see `VfoSettings::clock_ppm`).
static CLOCK_PPM: Mutex<f64> = Mutex::new(0.0);

fn prefs_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("DecDVB").join("prefs.txt"))
}

fn read_all() -> Vec<(String, String)> {
    let Some(p) = prefs_path() else {
        return Vec::new();
    };
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            l.split_once('=')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

fn write_key(key: &str, value: &str) -> std::io::Result<()> {
    let Some(p) = prefs_path() else {
        return Ok(());
    };
    let mut all = read_all();
    match all.iter_mut().find(|(k, _)| k == key) {
        Some(e) => e.1 = value.to_string(),
        None => all.push((key.to_string(), value.to_string())),
    }
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text: String = all.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    std::fs::write(p, text)
}

/// Documents\DecDVB, or the temp dir if there is no home.
fn fallback_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|b| PathBuf::from(b).join("Documents").join("DecDVB"))
        .unwrap_or_else(std::env::temp_dir)
}

/// Load the saved output folder (call once at start).
pub fn load() {
    let saved = read_all()
        .into_iter()
        .find(|(k, _)| k == "output_dir")
        .map(|(_, v)| PathBuf::from(v))
        .filter(|p| !p.as_os_str().is_empty());
    *OUTPUT_DIR.lock().unwrap() = saved;
    let all = read_all();
    let get = |key: &str| all.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
    if let Some(v) = get("volume").and_then(|v| v.parse::<f32>().ok()) {
        decdvb_audio::set_volume(v);
    }
    decdvb_audio::set_muted(get("muted") == Some("1"));
    if let Some(v) = get("clock_ppm").and_then(|v| v.parse::<f64>().ok()) {
        *CLOCK_PPM.lock().unwrap() = v;
    }
}

/// The clock correction, ppm.
pub fn clock_ppm() -> f64 {
    *CLOCK_PPM.lock().unwrap()
}

/// Use (and remember) a clock correction of `ppm`.
pub fn set_clock_ppm(ppm: f64) {
    *CLOCK_PPM.lock().unwrap() = ppm;
    let _ = write_key("clock_ppm", &format!("{ppm:.2}"));
}

/// Remember the audio volume and mute as they are now.
pub fn save_audio() {
    let _ = write_key("volume", &format!("{:.3}", decdvb_audio::volume()));
    let _ = write_key("muted", if decdvb_audio::muted() { "1" } else { "0" });
}

/// Where new output goes.
pub fn output_dir() -> PathBuf {
    OUTPUT_DIR
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(fallback_dir)
}

/// Use `dir` from now on, and remember it.
pub fn set_output_dir(dir: PathBuf) -> std::io::Result<()> {
    write_key("output_dir", &dir.to_string_lossy())?;
    *OUTPUT_DIR.lock().unwrap() = Some(dir);
    Ok(())
}
