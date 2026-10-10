//! A frequency plan: named carriers at RF frequencies, read from an SDR++
//! frequency-manager bookmark list —
//! `{"bookmarks": {"name": {"frequency": Hz, "bandwidth": Hz, "mode": n}}}`.
//! The waterfall labels the carriers it shows, and a VFO made on one takes
//! its name (and, made from the label, its width). The plan file is the
//! user's own: its path is remembered in the prefs, nothing of it is kept
//! anywhere else.

use std::path::Path;

/// One carrier of the plan.
#[derive(Debug, Clone, PartialEq)]
pub struct Bookmark {
    pub name: String,
    /// RF centre, Hz.
    pub freq_hz: f64,
    pub bandwidth_hz: f64,
}

/// The plan in `path`, sorted by frequency.
pub fn load(path: &Path) -> Result<Vec<Bookmark>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse(&text)
}

/// An SDR++ bookmark list's carriers, sorted by frequency.
pub fn parse(text: &str) -> Result<Vec<Bookmark>, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("not a JSON bookmark list: {e}"))?;
    let marks = v
        .get("bookmarks")
        .and_then(|b| b.as_object())
        .ok_or("no \"bookmarks\" in the file (an SDR++ frequency-manager list?)")?;
    let mut out: Vec<Bookmark> = marks
        .iter()
        .filter_map(|(name, m)| {
            Some(Bookmark {
                name: name.trim().to_string(),
                freq_hz: m.get("frequency")?.as_f64()?,
                bandwidth_hz: m.get("bandwidth").and_then(|b| b.as_f64()).unwrap_or(0.0),
            })
        })
        .collect();
    out.sort_by(|a, b| a.freq_hz.total_cmp(&b.freq_hz));
    Ok(out)
}

/// The plan's carrier a VFO centred at `rf_hz`, `bw_hz` wide, sits on: the
/// nearest whose centre lies inside the VFO (or the VFO's centre inside it).
pub fn at(plan: &[Bookmark], rf_hz: f64, bw_hz: f64) -> Option<&Bookmark> {
    plan.iter()
        .filter(|b| (b.freq_hz - rf_hz).abs() <= (bw_hz / 2.0).max(b.bandwidth_hz / 2.0))
        .min_by(|a, b| {
            (a.freq_hz - rf_hz)
                .abs()
                .total_cmp(&(b.freq_hz - rf_hz).abs())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_an_sdrpp_bookmark_list() {
        let plan = parse(
            r#"{"bookmarks":{"Site B":{"bandwidth":58837.25,"frequency":12352038770.0,"mode":1},
                "Site A ":{"bandwidth":15000.0,"frequency":12351836670.0,"mode":2},
                "no frequency":{"bandwidth":1.0}}}"#,
        )
        .unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].name, "Site A");
        assert_eq!(plan[1].freq_hz, 12_352_038_770.0);
        // A VFO near Site B's centre is on it; one between them on neither.
        assert_eq!(
            at(&plan, 12_352_040_000.0, 40_000.0).unwrap().name,
            "Site B"
        );
        assert!(at(&plan, 12_351_940_000.0, 20_000.0).is_none());
        assert!(parse("{}").is_err() && parse("nonsense").is_err());
    }
}
