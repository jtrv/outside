use crate::api::client;
use crate::api::location::LocationData;
use crate::utils;

use anyhow::{Context, Result};
use savefile::prelude::*;
use savefile_derive::Savefile;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Pollen changes daily, and the Google Pollen API bills per call past its free tier.
const CACHE_SECONDS: u64 = 3 * 60 * 60;

/// Open-Meteo pollen species (CAMS, Europe only) and the NAB group each belongs to.
const OPEN_METEO_SPECIES: [(&str, &str, &str); 6] = [
    ("alder_pollen", "Alder", "Tree"),
    ("birch_pollen", "Birch", "Tree"),
    ("olive_pollen", "Olive", "Tree"),
    ("grass_pollen", "Grass", "Grass"),
    ("mugwort_pollen", "Mugwort", "Weed"),
    ("ragweed_pollen", "Ragweed", "Weed"),
];

/// One pollen reading, normalized across providers.
///
/// `level` is on the provider's own scale (grains/m³ for Open-Meteo, 0–5 UPI for
/// Google); `category` is the comparable part.
#[derive(Default, Serialize, Deserialize, Debug, Clone, Savefile)]
pub struct PollenEntry {
    pub name: String,
    pub group: String,
    pub level: f64,
    pub category: String,
}

#[derive(Default, Serialize, Deserialize, Debug, Clone, Savefile)]
pub struct Pollen {
    pub source: String,
    pub entries: Vec<PollenEntry>,
    /// Plants currently driving the reading, by name only.
    pub triggers: Vec<String>,
}

#[derive(Default, Savefile)]
struct CachedPollen {
    key: String,
    created_at: u64,
    pollen: Option<Pollen>,
}

impl Pollen {
    /// Returns pollen data for the location, or `None` when no provider covers it
    /// or every provider request fails.
    ///
    /// Provider order: Google when `google_api_key_file` holds a key, otherwise
    /// Open-Meteo (Europe only).
    pub fn get_cached(loc: &LocationData, google_api_key_file: &str) -> Option<Self> {
        let google_api_key = read_api_key(google_api_key_file);
        let google_api_key = google_api_key.as_str();
        let key = format!("{},{},{}", loc.latitude, loc.longitude, !google_api_key.is_empty());
        let filename = utils::cache::get_cached_file("pollen", &key);
        let now = utils::get_now();

        let cached: CachedPollen = load_file(&filename, 0).unwrap_or_default();
        if cached.key == key && cached.created_at > 0 && now - cached.created_at < CACHE_SECONDS {
            return cached.pollen;
        }

        // A failed request is not cached, so the next run retries; "no coverage" is.
        let pollen = Self::fetch(loc, google_api_key).ok()?;
        let entry = CachedPollen { key, created_at: now, pollen };
        if let Err(e) = save_file(&filename, 0, &entry) {
            eprintln!("Unable to save pollen data to disk: {e:#?}");
        }
        entry.pollen
    }

    /// A one-line summary such as "Tree Low · Weed High · Ragweed".
    pub fn description(&self) -> String {
        let notable: Vec<String> = self
            .entries
            .iter()
            .filter(|e| e.category != "None")
            .map(|e| format!("{} {}", e.name, e.category))
            .collect();
        let summary = if notable.is_empty() { "None".to_string() } else { notable.join(" · ") };

        if self.triggers.is_empty() {
            summary
        } else {
            format!("{summary} · {}", self.triggers.join(", "))
        }
    }

    fn fetch(loc: &LocationData, google_api_key: &str) -> Result<Option<Self>> {
        if !google_api_key.is_empty() {
            return Self::fetch_google(loc.latitude, loc.longitude, google_api_key);
        }
        Self::fetch_open_meteo(loc.latitude, loc.longitude)
    }

    fn fetch_open_meteo(lat: f64, lon: f64) -> Result<Option<Self>> {
        let lat_str = lat.to_string();
        let lon_str = lon.to_string();
        let species = OPEN_METEO_SPECIES.map(|(field, _, _)| field).join(",");
        let api_url = utils::urls::builder(
            "https://air-quality-api.open-meteo.com/v1/air-quality",
            vec![("latitude", lat_str.as_str()), ("longitude", lon_str.as_str()), ("current", &species)],
        );
        let body = client::get_with_retry(&api_url, 2)
            .with_context(|| "Unable to fetch pollen data from the Open-Meteo API endpoint")?;
        Self::parse_open_meteo(&body)
    }

    fn parse_open_meteo(body: &str) -> Result<Option<Self>> {
        let json: Value =
            serde_json::from_str(body).with_context(|| "Unable to parse Open-Meteo pollen JSON")?;
        let current = &json["current"];

        let entries: Vec<PollenEntry> = OPEN_METEO_SPECIES
            .iter()
            .filter_map(|&(field, name, group)| {
                let level = current[field].as_f64()?;
                Some(PollenEntry {
                    name: name.to_string(),
                    group: group.to_string(),
                    level,
                    category: nab_category(group, level).to_string(),
                })
            })
            .collect();

        // Outside the CAMS European domain every species comes back null.
        if entries.is_empty() {
            return Ok(None);
        }
        Ok(Some(Pollen { source: "open-meteo".to_string(), entries, triggers: Vec::new() }))
    }

    fn fetch_google(lat: f64, lon: f64, api_key: &str) -> Result<Option<Self>> {
        let lat_str = lat.to_string();
        let lon_str = lon.to_string();
        let api_url = utils::urls::builder(
            "https://pollen.googleapis.com/v1/forecast:lookup",
            vec![
                ("location.latitude", lat_str.as_str()),
                ("location.longitude", lon_str.as_str()),
                ("days", "1"),
                ("plantsDescription", "0"),
            ],
        );
        // A header rather than the documented ?key= parameter, because request
        // errors include the URL and would print the key.
        let body = client::get_with_headers(&api_url, &[("X-Goog-Api-Key", api_key)])
            .with_context(|| "Unable to fetch pollen data from the Google Pollen API")?;
        Self::parse_google(&body)
    }

    fn parse_google(body: &str) -> Result<Option<Self>> {
        let json: Value = serde_json::from_str(body).with_context(|| "Unable to parse Google pollen JSON")?;
        let today = &json["dailyInfo"][0];
        let Some(types) = today["pollenTypeInfo"].as_array() else {
            return Ok(None);
        };

        // indexInfo is omitted for out-of-season types.
        let entries = types
            .iter()
            .map(|t| PollenEntry {
                name: t["displayName"].as_str().unwrap_or_default().to_string(),
                group: t["displayName"].as_str().unwrap_or_default().to_string(),
                level: t["indexInfo"]["value"].as_f64().unwrap_or(0.0),
                category: t["indexInfo"]["category"].as_str().unwrap_or("None").to_string(),
            })
            .collect();

        let triggers = today["plantInfo"]
            .as_array()
            .map(|plants| {
                plants
                    .iter()
                    .filter(|p| p["indexInfo"]["value"].as_f64().unwrap_or(0.0) > 0.0)
                    .filter_map(|p| p["displayName"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        Ok(Some(Pollen { source: "google".to_string(), entries, triggers }))
    }
}

/// Reads an API key from a file, so config.yaml can be committed without it.
/// An empty path means no key.
fn read_api_key(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    std::fs::read_to_string(path).map(|key| key.trim().to_string()).unwrap_or_else(|e| {
        eprintln!("Unable to read the Google Pollen API key from {path}: {e}");
        String::new()
    })
}

/// US National Allergy Bureau count categories (grains/m³), per plant group.
fn nab_category(group: &str, grains: f64) -> &'static str {
    let (low, moderate, high) = match group {
        "Tree" => (15.0, 90.0, 1500.0),
        "Grass" => (5.0, 20.0, 200.0),
        _ => (10.0, 50.0, 500.0),
    };
    match grains {
        g if g < 1.0 => "None",
        g if g < low => "Low",
        g if g < moderate => "Moderate",
        g if g < high => "High",
        _ => "Very High",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_meteo_outside_europe_is_none() {
        let body = r#"{"current":{"time":"2026-10-07T13:00","alder_pollen":null,"grass_pollen":null}}"#;
        assert!(Pollen::parse_open_meteo(body).unwrap().is_none());
    }

    #[test]
    fn open_meteo_maps_species_to_nab_categories() {
        let body =
            r#"{"current":{"alder_pollen":0,"birch_pollen":120.5,"grass_pollen":4,"ragweed_pollen":null}}"#;
        let pollen = Pollen::parse_open_meteo(body).unwrap().unwrap();
        let categories: Vec<_> =
            pollen.entries.iter().map(|e| (e.name.as_str(), e.category.as_str())).collect();
        assert_eq!(categories, [("Alder", "None"), ("Birch", "High"), ("Grass", "Low")]);
        assert_eq!(pollen.description(), "Birch High · Grass Low");
    }

    #[test]
    fn google_reads_types_and_in_season_plants() {
        let body = r#"{"regionCode":"US","dailyInfo":[{
            "pollenTypeInfo":[
                {"code":"GRASS","displayName":"Grass","indexInfo":{"value":1,"category":"Very Low"}},
                {"code":"TREE","displayName":"Tree","inSeason":false},
                {"code":"WEED","displayName":"Weed","indexInfo":{"value":3,"category":"Moderate"}}],
            "plantInfo":[
                {"code":"BIRCH","displayName":"Birch","indexInfo":{"value":0,"category":"None"}},
                {"code":"RAGWEED","displayName":"Ragweed","indexInfo":{"value":3,"category":"Moderate"}}]}]}"#;
        let pollen = Pollen::parse_google(body).unwrap().unwrap();
        assert_eq!(pollen.description(), "Grass Very Low · Weed Moderate · Ragweed");
    }

    #[test]
    fn category_boundaries() {
        assert_eq!(nab_category("Grass", 4.9), "Low");
        assert_eq!(nab_category("Grass", 5.0), "Moderate");
        assert_eq!(nab_category("Weed", 500.0), "Very High");
    }
}
