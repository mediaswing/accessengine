//! Turning a GPS coordinate from a photo's EXIF data into a place name.
//!
//! Nothing else on the image-reading path leaves this computer — the vision
//! model runs locally through Ollama — but there is no local database of
//! place names, so this one lookup goes out to OpenStreetMap's Nominatim
//! service, and the coordinate is all it receives: no image, no filename, no
//! account.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::time::Duration;

const ENDPOINT: &str = "https://nominatim.openstreetmap.org/reverse";

/// City-level detail. A house number is more precision than "where was this
/// picture taken" calls for, and than a photo's own GPS accuracy usually
/// supports.
const ZOOM: &str = "10";

fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        // Nominatim's usage policy requires a way to identify the client,
        // since requests aren't otherwise authenticated.
        .user_agent(concat!("speech-output-engine/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("could not create an HTTP client")
}

#[derive(Deserialize, Default)]
struct ReverseResponse {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    address: Option<Address>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize, Default)]
struct Address {
    #[serde(default)]
    city: Option<String>,
    #[serde(default)]
    town: Option<String>,
    #[serde(default)]
    village: Option<String>,
    #[serde(default)]
    municipality: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    country: Option<String>,
}

/// Looks up a spoken-friendly place name for a coordinate, e.g. "San
/// Francisco, California, United States".
pub fn place_name(latitude: f64, longitude: f64) -> Result<String> {
    // Built by hand rather than through reqwest's `query`, which would pull
    // in a feature this app otherwise has no use for: every value here is
    // either a fixed string or an `f64`, and Rust's `f64` never renders in a
    // form a URL query needs escaping for (no `&`, `=`, `#`, or `%`).
    let url = format!("{ENDPOINT}?format=jsonv2&lat={latitude}&lon={longitude}&zoom={ZOOM}");

    let response: ReverseResponse = client()?
        .get(url)
        .send()
        .context("could not reach the location lookup service")?
        .error_for_status()
        .context("the location lookup service returned an error")?
        .json()
        .context("the location lookup service returned an unexpected response")?;

    if let Some(error) = &response.error {
        bail!("the location lookup service could not place this coordinate: {error}");
    }
    describe(&response).context("the location lookup service had no name for this coordinate")
}

/// Prefers a locality plus state and country over Nominatim's full
/// `display_name`, which reads out street numbers and postcodes that mean
/// nothing spoken aloud.
fn describe(response: &ReverseResponse) -> Option<String> {
    if let Some(address) = &response.address {
        let locality = address
            .city
            .clone()
            .or_else(|| address.town.clone())
            .or_else(|| address.village.clone())
            .or_else(|| address.municipality.clone());
        let parts: Vec<String> = [locality, address.state.clone(), address.country.clone()]
            .into_iter()
            .flatten()
            .collect();
        if !parts.is_empty() {
            return Some(parts.join(", "));
        }
    }
    response.display_name.clone()
}

#[cfg(test)]
mod tests {
    use super::{Address, ReverseResponse, describe};

    #[test]
    fn prefers_locality_state_and_country_over_the_full_address() {
        let response = ReverseResponse {
            display_name: Some("123 Market St, San Francisco, CA 94103, USA".to_string()),
            address: Some(Address {
                city: Some("San Francisco".to_string()),
                state: Some("California".to_string()),
                country: Some("United States".to_string()),
                ..Default::default()
            }),
            error: None,
        };
        assert_eq!(
            describe(&response),
            Some("San Francisco, California, United States".to_string())
        );
    }

    #[test]
    fn falls_back_through_town_village_and_municipality() {
        let town = Address {
            town: Some("Ambleside".to_string()),
            ..Default::default()
        };
        assert_eq!(
            describe(&ReverseResponse {
                address: Some(town),
                ..Default::default()
            }),
            Some("Ambleside".to_string())
        );

        let village = Address {
            village: Some("Grasmere".to_string()),
            ..Default::default()
        };
        assert_eq!(
            describe(&ReverseResponse {
                address: Some(village),
                ..Default::default()
            }),
            Some("Grasmere".to_string())
        );
    }

    #[test]
    fn falls_back_to_display_name_when_the_address_has_no_useful_fields() {
        let response = ReverseResponse {
            display_name: Some("somewhere in the ocean".to_string()),
            address: Some(Address::default()),
            error: None,
        };
        assert_eq!(
            describe(&response),
            Some("somewhere in the ocean".to_string())
        );
    }

    #[test]
    fn no_address_and_no_display_name_describes_nothing() {
        assert_eq!(describe(&ReverseResponse::default()), None);
    }
}
