// SPDX-License-Identifier: Apache-2.0

//! Download contracts for retained original-file assets.

use std::collections::BTreeMap;

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct AssetDownloadContract {
    pub asset_id: String,
    pub download_url: String,
    pub download_path: String,
    pub route_kind: String,
    pub download_method: String,
    pub required_headers: BTreeMap<String, String>,
    pub download_auth: AssetDownloadAuthContract,
    pub filename: Option<String>,
    pub mime_type: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub etag: String,
    pub expires_at_ms: Option<i64>,
    pub next_actions: Vec<AssetDownloadNextAction>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssetDownloadAuthContract {
    pub mode: String,
    pub required: String,
    pub header: String,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssetDownloadNextAction {
    pub action: String,
    pub transport: String,
    pub method: Option<String>,
    pub url_field: Option<String>,
    pub headers_field: Option<String>,
    pub when: Option<String>,
}

pub fn direct_asset_download_contract(
    target: &solo_query::AssetDownloadTarget,
) -> AssetDownloadContract {
    let download_path = format!("/memory/assets/{}/download", target.asset.asset_id);
    AssetDownloadContract {
        asset_id: target.asset.asset_id.clone(),
        download_url: download_path.clone(),
        download_path,
        route_kind: "direct_local".to_string(),
        download_method: "GET".to_string(),
        required_headers: BTreeMap::new(),
        download_auth: AssetDownloadAuthContract {
            mode: "same_as_solo_http".to_string(),
            required: "when the Solo HTTP API is configured with auth".to_string(),
            header: "authorization".to_string(),
            note: "Direct Solo HTTP downloads use the same Authorization bearer as the rest of the Solo API and do not mint a separate local download token.".to_string(),
        },
        filename: target.asset.filename.clone(),
        mime_type: target.asset.mime_type.clone(),
        size_bytes: target.asset.size_bytes,
        sha256: target.asset.sha256.clone(),
        etag: format!("\"{}\"", target.asset.sha256),
        expires_at_ms: None,
        next_actions: vec![AssetDownloadNextAction {
            action: "download_bytes".to_string(),
            transport: "raw_http".to_string(),
            method: Some("GET".to_string()),
            url_field: Some("download_url".to_string()),
            headers_field: Some("required_headers".to_string()),
            when: Some("after user/client policy authorizes raw file access".to_string()),
        }],
    }
}
