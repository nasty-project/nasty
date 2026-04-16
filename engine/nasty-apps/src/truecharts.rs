//! TrueCharts catalog — fetches and caches the chart list from truecharts.org
//! for browsing in the WebUI. Installation happens via the existing BYOH
//! helm install path using `oci://tccr.io/truecharts/<name>:<version>`.
//!
//! The upstream site publishes HTML only (no JSON/YAML API), so we scrape
//! the description list page. Cache lives at /var/lib/nasty/truecharts-index.json
//! and is refreshed on-demand or on a periodic schedule.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

const SOURCE_URL: &str =
    "https://truecharts.org/truetech/truecharts/charts/description-list/";
const CACHE_PATH: &str = "/var/lib/nasty/truecharts-index.json";
pub const OCI_REGISTRY: &str = "oci://oci.trueforge.org/truecharts";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TrueChartEntry {
    pub name: String,
    pub version: String,
    pub train: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct TrueChartsIndex {
    /// Unix timestamp of last successful fetch.
    pub refreshed_at: u64,
    pub charts: Vec<TrueChartEntry>,
}

/// Return the cached index, or an empty one if the cache is missing.
pub async fn load_cache() -> TrueChartsIndex {
    let content = tokio::fs::read_to_string(CACHE_PATH).await.unwrap_or_default();
    serde_json::from_str(&content).unwrap_or_default()
}

/// Fetch upstream, parse, and save to cache. Returns the fresh index on success.
pub async fn refresh() -> Result<TrueChartsIndex, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("nasty-engine")
        .build()
        .map_err(|e| e.to_string())?;

    let html = client
        .get(SOURCE_URL)
        .send()
        .await
        .map_err(|e| format!("fetch failed: {e}"))?
        .text()
        .await
        .map_err(|e| format!("read body failed: {e}"))?;

    let charts = parse_charts(&html);
    if charts.is_empty() {
        return Err("parsed zero charts — upstream HTML structure may have changed".into());
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let index = TrueChartsIndex { refreshed_at: now, charts };

    let json = serde_json::to_string(&index).map_err(|e| e.to_string())?;
    tokio::fs::write(CACHE_PATH, json).await.map_err(|e| e.to_string())?;

    info!("TrueCharts index refreshed: {} charts", index.charts.len());
    Ok(index)
}

/// Parse chart rows from the HTML. Structure (one row per chart):
///   <tr data-chart="NAME" data-train="TRAIN"> ...
///     <td ...> VERSION </td>
///     <td data-description ...> DESCRIPTION </td>
///   </tr>
fn parse_charts(html: &str) -> Vec<TrueChartEntry> {
    let mut out = Vec::new();
    let mut cursor = 0;

    while let Some(start) = html[cursor..].find("<tr data-chart=\"") {
        let row_start = cursor + start;
        let row_end = html[row_start..]
            .find("</tr>")
            .map(|e| row_start + e)
            .unwrap_or(html.len());
        let row = &html[row_start..row_end];
        cursor = row_end;

        let Some(name) = attr(row, "data-chart") else { continue };
        let Some(train) = attr(row, "data-train") else { continue };

        // Columns are: Chart | Version | Description. Take the 2nd <td> body
        // for version; description is identified by the `data-description` attr.
        let version = nth_td_body(row, 1).unwrap_or_default();
        let description = extract_description(row).unwrap_or_default();

        out.push(TrueChartEntry {
            name: name.to_string(),
            version,
            train: train.to_string(),
            description,
        });
    }

    out
}

fn attr<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=\"");
    let start = s.find(&needle)? + needle.len();
    let end = s[start..].find('"')? + start;
    Some(&s[start..end])
}

/// Return the inner text of the Nth `<td>` in the row (0-indexed).
fn nth_td_body(row: &str, n: usize) -> Option<String> {
    let mut cursor = 0;
    let mut i = 0;
    while let Some(td) = row[cursor..].find("<td") {
        let td_start = cursor + td;
        let tag_end = row[td_start..].find('>')? + td_start;
        let close = row[tag_end..].find("</td>")? + tag_end;
        let body = &row[tag_end + 1..close];
        if i == n {
            return Some(body.trim().to_string());
        }
        i += 1;
        cursor = close;
    }
    None
}

fn extract_description(row: &str) -> Option<String> {
    let marker = "data-description";
    let pos = row.find(marker)?;
    let tag_end = row[pos..].find('>')? + pos;
    let close = row[tag_end..].find("</td>")? + tag_end;
    Some(row[tag_end + 1..close].trim().to_string())
}

/// Spawn a background task that refreshes the cache now (best-effort) and
/// then once per day.
pub fn spawn_periodic_refresh() {
    tokio::spawn(async {
        // Small initial delay so bootstrap / DNS / networking settle first.
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;

        loop {
            match refresh().await {
                Ok(idx) => info!(
                    "TrueCharts index ready ({} charts)",
                    idx.charts.len()
                ),
                Err(e) => warn!("TrueCharts refresh failed: {e}"),
            }
            // Retry daily. On failure we still have the last good cache.
            tokio::time::sleep(std::time::Duration::from_secs(24 * 3600)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<tr data-chart="valkey" data-train="incubator"> <td class="text-lg max-md:text-base font-normal text-balance max-md:text-center"> <div class="flex flex-row max-md:flex-col gap-2 items-center justify-start"> <img src="/x.webp" alt="valkey"> <a href="/x"> valkey </a> </div> </td> <td class="text-lg font-normal text-wrap text-center max-md:text-base"> 1.0.6 </td> <td data-description class="text-lg font-normal text-balance text-left max-md:text-base"> Open source, advanced key-value store. </td> </tr>
<tr data-chart="jellyfin" data-train="stable"> <td class="text-lg"> <div><img><a href="/x"> jellyfin </a></div> </td> <td class="text-center"> 23.0.8 </td> <td data-description class="text-lg"> Free Software media system </td> </tr>"#;

    #[test]
    fn parses_rows() {
        let charts = parse_charts(SAMPLE);
        assert_eq!(charts.len(), 2);
        assert_eq!(charts[0].name, "valkey");
        assert_eq!(charts[0].train, "incubator");
        assert_eq!(charts[0].version, "1.0.6");
        assert_eq!(charts[0].description, "Open source, advanced key-value store.");
        assert_eq!(charts[1].name, "jellyfin");
        assert_eq!(charts[1].version, "23.0.8");
        assert_eq!(charts[1].description, "Free Software media system");
    }
}
