//! Live satellite layer from CelesTrak general-perturbations (OMM JSON) element sets,
//! propagated with SGP4 to the current time. `commercial_clean` (CelesTrak asks for
//! attribution and a polite fetch cadence — element sets are refreshed at most every few
//! hours, so the server caches them).

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

pub const DEFAULT_GROUP: &str = "active";
const EARTH_RADIUS_KM: f64 = 6371.0;

pub async fn fetch_group(group: &str) -> Result<Vec<sgp4::Elements>> {
    let client = reqwest::Client::builder()
        .user_agent(crate::USER_AGENT)
        .build()?;
    let url = format!("https://celestrak.org/NORAD/elements/gp.php?GROUP={group}&FORMAT=json");
    let text = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("CelesTrak request failed: {url}"))?
        .error_for_status()?
        .text()
        .await?;
    let elements: Vec<sgp4::Elements> = serde_json::from_str(&text)
        .with_context(|| format!("CelesTrak returned non-OMM JSON for group {group:?}"))?;
    Ok(elements)
}

#[derive(Debug, Clone)]
pub struct SatPosition {
    pub name: String,
    pub norad: u64,
    pub lat: f64,
    pub lon: f64,
    pub alt_km: f64,
}

/// Greenwich mean sidereal time (radians) for a UTC instant — IAU 1982 polynomial, ample for
/// placing points on a globe.
fn gmst_radians(at: chrono::DateTime<chrono::Utc>) -> f64 {
    let jd = at.timestamp() as f64 / 86_400.0
        + 2_440_587.5
        + (at.timestamp_subsec_millis() as f64 / 86_400_000.0);
    let t = (jd - 2_451_545.0) / 36_525.0;
    let seconds = 67_310.548_41 + (876_600.0 * 3_600.0 + 8_640_184.812_866) * t + 0.093_104 * t * t
        - 6.2e-6 * t * t * t;
    let seconds = seconds.rem_euclid(86_400.0);
    (seconds / 240.0).to_radians()
}

/// TEME position (km) → geodetic-ish lat/lon (degrees) and altitude (km), spherical Earth.
fn teme_to_geo(pos: [f64; 3], at: chrono::DateTime<chrono::Utc>) -> (f64, f64, f64) {
    let g = gmst_radians(at);
    let (x, y, z) = (pos[0], pos[1], pos[2]);
    let xe = x * g.cos() + y * g.sin();
    let ye = -x * g.sin() + y * g.cos();
    let r = (x * x + y * y + z * z).sqrt();
    let lat = (z / r).asin().to_degrees();
    let lon = ye.atan2(xe).to_degrees();
    (lat, lon, r - EARTH_RADIUS_KM)
}

/// Propagates every element set to `at`. Objects whose elements fail to propagate (decayed,
/// out-of-range eccentricity) are skipped.
pub fn positions(
    elements: &[sgp4::Elements],
    at: chrono::DateTime<chrono::Utc>,
) -> Vec<SatPosition> {
    let naive = at.naive_utc();
    elements
        .iter()
        .filter_map(|e| {
            let constants = sgp4::Constants::from_elements(e).ok()?;
            let minutes = e.datetime_to_minutes_since_epoch(&naive).ok()?;
            let pred = constants.propagate(minutes).ok()?;
            let (lat, lon, alt_km) = teme_to_geo(pred.position, at);
            if !lat.is_finite() || !lon.is_finite() {
                return None;
            }
            Some(SatPosition {
                name: e
                    .object_name
                    .clone()
                    .unwrap_or_else(|| format!("NORAD {}", e.norad_id)),
                norad: e.norad_id,
                lat,
                lon,
                alt_km,
            })
        })
        .collect()
}

pub fn to_geojson(sats: &[SatPosition], group: &str, at: chrono::DateTime<chrono::Utc>) -> Value {
    let feats: Vec<Value> = sats
        .iter()
        .map(|s| {
            json!({
                "type": "Feature",
                "geometry": {"type": "Point", "coordinates": [s.lon, s.lat, s.alt_km * 1000.0]},
                "properties": {"name": s.name, "norad": s.norad, "alt_km": s.alt_km}
            })
        })
        .collect();
    json!({
        "type": "FeatureCollection",
        "properties": {"group": group, "at": at.to_rfc3339(), "count": feats.len(), "source": "CelesTrak", "license_class": "commercial_clean"},
        "features": feats
    })
}

pub fn check_group(group: &str) -> Result<()> {
    if group.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') && !group.is_empty() {
        Ok(())
    } else {
        Err(anyhow!("invalid CelesTrak group name {group:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gmst_is_within_a_turn() {
        let g = gmst_radians(chrono::Utc::now());
        assert!((0.0..std::f64::consts::TAU).contains(&g));
    }

    #[test]
    fn iss_propagates_to_low_earth_orbit() {
        let omm = r#"[{"OBJECT_NAME":"ISS (ZARYA)","OBJECT_ID":"1998-067A","EPOCH":"2026-09-08T23:34:28.526304","MEAN_MOTION":15.49061543,"ECCENTRICITY":0.00048547,"INCLINATION":51.6292,"RA_OF_ASC_NODE":245.3706,"ARG_OF_PERICENTER":110.6068,"MEAN_ANOMALY":249.5441,"EPHEMERIS_TYPE":0,"CLASSIFICATION_TYPE":"U","NORAD_CAT_ID":25544,"ELEMENT_SET_NO":999,"REV_AT_EPOCH":58474,"BSTAR":0.00025209066,"MEAN_MOTION_DOT":0.00013495,"MEAN_MOTION_DDOT":0}]"#;
        let elements: Vec<sgp4::Elements> = serde_json::from_str(omm).unwrap();
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-09T01:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let sats = positions(&elements, at);
        assert_eq!(sats.len(), 1);
        let s = &sats[0];
        assert!(
            s.alt_km > 350.0 && s.alt_km < 480.0,
            "ISS altitude {}",
            s.alt_km
        );
        assert!(
            s.lat.abs() <= 52.0,
            "ISS latitude bounded by inclination: {}",
            s.lat
        );
        assert_eq!(s.norad, 25544);
    }
}
