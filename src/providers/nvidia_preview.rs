//! NVIDIA's public Build catalog labels hosted previews "Free Endpoint".
//! The inference API omits this metadata. Read the site's filtered, paginated
//! result rather than inferring from the first page or from model names alone.
use serde::Deserialize;
use std::{collections::HashSet, time::Duration};

pub(super) const URL: &str = "https://build.nvidia.com/models?filters=nimType%3Anim_type_preview";
const PREVIEW: &str = "nim_type_preview";
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_PAGES: usize = 8;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page {
    params: Params,
    result_page_total: usize,
    result_total: usize,
    results: Vec<Group>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Params {
    page: usize,
    page_size: usize,
    filters: Vec<Filter>,
}

#[derive(Deserialize)]
struct Filter {
    field: String,
    value: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Group {
    group_value: String,
    total_count: usize,
    resources: Vec<Resource>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Resource {
    resource_id: String,
    name: String,
    labels: Vec<Label>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Label {
    key: String,
    unresolved_values: Vec<String>,
}

impl Resource {
    fn labels(&self, key: &str) -> impl Iterator<Item = &str> {
        self.labels
            .iter()
            .filter(move |label| label.key == key)
            .flat_map(|label| label.unresolved_values.iter().map(String::as_str))
    }
}

fn parse_page(html: &str) -> Result<Page, String> {
    // Flight records can span script tags. Decode their JSON strings before
    // parsing the searchResult object so split objects and escaped text work.
    let mut payload = String::new();
    for script in html.split("self.__next_f.push(").skip(1) {
        let Some((call, _)) = script.split_once("</script>") else {
            continue;
        };
        let Some(args) = call.trim().strip_suffix(')') else {
            continue;
        };
        let Ok(args) = serde_json::from_str::<Vec<serde_json::Value>>(args) else {
            continue;
        };
        if let Some(s) = args.get(1).and_then(serde_json::Value::as_str) {
            payload.push_str(s);
        }
    }
    payload
        .split("\"searchResult\":")
        .skip(1)
        .find_map(|part| {
            serde_json::Deserializer::from_str(part)
                .into_iter::<Page>()
                .next()?
                .ok()
        })
        .ok_or_else(|| "NVIDIA Build page has no recognizable searchResult".into())
}

async fn get_page(client: &reqwest::Client, url: &str) -> Result<Page, String> {
    let mut response = client
        .get(url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(format!("NVIDIA Build returned HTTP {}", response.status()));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if bytes.len() + chunk.len() > MAX_BYTES {
            return Err("NVIDIA Build page exceeded size limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    parse_page(std::str::from_utf8(&bytes).map_err(|e| e.to_string())?)
}

pub(super) async fn free_ids(
    client: &reqwest::Client,
    url: &str,
) -> Result<HashSet<String>, String> {
    let first = get_page(client, url).await?;
    let total = first.result_total;
    let page_size = first.params.page_size;
    let pages = first.result_page_total;
    if page_size == 0 || pages > MAX_PAGES || pages != total.div_ceil(page_size).max(1) {
        return Err("invalid or oversized NVIDIA preview pagination".into());
    }
    let mut seen = HashSet::new();
    let mut ids = HashSet::new();
    let mut collect = |page: Page, index: usize| {
        if page.params.page != index
            || page.params.page_size != page_size
            || page.result_page_total != pages
            || page.result_total != total
            || !page
                .params
                .filters
                .iter()
                .any(|filter| filter.field == "nimType" && filter.value == PREVIEW)
        {
            return Err("NVIDIA preview pagination or filter changed during fetch".to_string());
        }
        if total == 0 {
            return Ok(());
        }
        let group = page
            .results
            .into_iter()
            .find(|group| group.group_value == "ENDPOINT")
            .ok_or("NVIDIA preview page has no ENDPOINT group")?;
        if group.total_count != total
            || group.resources.len() != (total - index * page_size).min(page_size)
        {
            return Err("incomplete NVIDIA preview page".into());
        }
        for resource in group.resources {
            if !resource.labels("nimType").any(|flag| flag == PREVIEW) {
                return Err("NVIDIA preview result missing Free Endpoint flag".into());
            }
            for publisher in resource.labels("publisher") {
                if !publisher.is_empty()
                    && !publisher.contains('/')
                    && !resource.name.is_empty()
                    && !resource.name.contains('/')
                {
                    ids.insert(format!("{publisher}/{}", resource.name));
                }
            }
            if resource.resource_id.is_empty() || !seen.insert(resource.resource_id) {
                return Err("missing or duplicate NVIDIA preview resource ID".into());
            }
        }
        Ok(())
    };
    collect(first, 0)?;
    let remaining = futures::future::try_join_all(
        (2..=pages)
            .map(|page| async move { get_page(client, &format!("{url}&page={page}")).await }),
    )
    .await?;
    for (index, page) in remaining.into_iter().enumerate() {
        collect(page, index + 1)?;
    }
    if seen.len() != total {
        return Err("incomplete NVIDIA preview catalog".into());
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, extract::Query, http::HeaderMap, response::Html, routing::get};
    use serde_json::{Value, json};

    fn page(page: usize, repeat: bool) -> String {
        let model = if page == 0 || repeat {
            "burst"
        } else {
            "steady"
        };
        let result = json!({
            "params": {"page": page, "pageSize": 1,
                "filters": [{"field": "nimType", "value": "nim_type_preview"}]},
            "resultPageTotal": 2,
            "resultTotal": 2,
            "results": [
                {"groupValue": "_scored", "totalCount": 1, "resources": []},
                {"groupValue": "ENDPOINT", "totalCount": 2, "resources": [{
                    "resourceType": "ENDPOINT",
                    "resourceId": format!("catalog/{model}"),
                    "name": model,
                    "labels": [
                        {"key": "nimType", "values": ["Free Endpoint"], "unresolvedValues": ["nim_type_preview"]},
                        {"key": "publisher", "values": ["NVIDIA"], "unresolvedValues": ["nvidia"]}
                    ]
                }]}
            ]
        });
        let flight = json!(["$","component",null,{"searchResult": result}]);
        format!(
            "<script>self.__next_f.push({})</script>",
            json!([1, flight.to_string()])
        )
    }

    async fn catalog(repeat: bool) -> String {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/models",
            get(
                move |Query(query): Query<std::collections::HashMap<String, String>>,
                      headers: HeaderMap| async move {
                    assert_eq!(query["filters"], "nimType:nim_type_preview");
                    assert!(!headers.contains_key("authorization"));
                    let index = match query.get("page").map(String::as_str) {
                        None => 0,
                        Some("2") => 1,
                        other => panic!("unexpected page: {other:?}"),
                    };
                    Html(page(index, repeat))
                },
            ),
        );
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}/models?filters=nimType%3Anim_type_preview")
    }

    #[tokio::test]
    async fn fetches_every_preview_page() {
        let url = catalog(false).await;
        let found = free_ids(&reqwest::Client::new(), &url).await.unwrap();
        assert_eq!(
            found,
            HashSet::from(["nvidia/burst".into(), "nvidia/steady".into()])
        );
    }

    #[tokio::test]
    async fn rejects_partial_catalog_with_duplicate_resource() {
        let url = catalog(true).await;
        let error = free_ids(&reqwest::Client::new(), &url).await.unwrap_err();
        assert!(error.contains("duplicate"), "{error}");
    }

    #[tokio::test]
    #[ignore = "uses NVIDIA's public endpoints"]
    async fn live_free_endpoint_catalog() {
        let client = crate::providers::default_http_client();
        let free = free_ids(&client, URL).await.unwrap();
        let page: Value = client
            .get("https://integrate.api.nvidia.com/v1/models")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let mut joined: Vec<_> = page["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|model| model["id"].as_str())
            .filter(|id| free.contains(*id))
            .collect();
        joined.sort_unstable();
        eprintln!(
            "Free Endpoint named entries: {}; exact inference IDs: {}: {:?}",
            free.len(),
            joined.len(),
            joined
        );
        assert!(!joined.is_empty());
    }
}
