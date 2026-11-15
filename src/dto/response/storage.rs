use crate::dto::PaginatedResponse;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StorageResponse {
    pub id: u16,
    pub name: String,
    pub path: String,
    pub r#type: String,
    pub scheme: String,
    pub server_id: Option<u32>,
    pub url: Option<String>,
    /// Configured capacity in bytes (`Storages.DiskSpace`), if set.
    pub disk_space: Option<i64>,
    pub do_delete: i8,
    pub enabled: i8,
    /// The filesystem the storage's path is on, as `df` reports it: the
    /// disk's own size and use, not the capacity declared for retention. Null
    /// when the path can't be read from this server (#145).
    pub filesystem: Option<StorageFilesystem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StorageFilesystem {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub usage_percent: f64,
}

impl StorageResponse {
    /// For a caller who may view storage but not edit it: the URL without
    /// its password. An S3 area's is its secret key
    /// (`s3fs://KEY:SECRET@host/bucket`).
    pub fn without_secrets(mut self) -> Self {
        self.url = self
            .url
            .as_deref()
            .map(crate::dto::response::monitor::hide_url_password);
        self
    }

    /// Fill `filesystem` from the storage path (a `statvfs`).
    pub fn with_filesystem(mut self) -> Self {
        self.filesystem = crate::daemon::stats::filesystem_usage(std::path::Path::new(&self.path))
            .ok()
            .map(|u| StorageFilesystem {
                total_bytes: u.total_bytes,
                used_bytes: u.used_bytes,
                available_bytes: u.available_bytes,
                usage_percent: u.usage_percent,
            });
        self
    }
}

impl From<&crate::entity::storage::Model> for StorageResponse {
    fn from(m: &crate::entity::storage::Model) -> Self {
        Self {
            id: m.id,
            name: m.name.clone(),
            path: m.path.clone(),
            r#type: m.r#type.to_string(),
            scheme: serde_json::to_value(&m.scheme)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default(),
            server_id: m.server_id,
            url: m.url.clone(),
            disk_space: m.disk_space,
            do_delete: m.do_delete,
            enabled: m.enabled,
            filesystem: None,
        }
    }
}

/// Paginated response for storage
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct PaginatedStorageResponse {
    pub items: Vec<StorageResponse>,
    pub total: u64,
    pub per_page: u64,
    pub current_page: u64,
    pub last_page: u64,
}

impl From<PaginatedResponse<StorageResponse>> for PaginatedStorageResponse {
    fn from(r: PaginatedResponse<StorageResponse>) -> Self {
        Self {
            items: r.items,
            total: r.total,
            per_page: r.per_page,
            current_page: r.current_page,
            last_page: r.last_page,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_viewer_does_not_get_the_s3_secret_key() {
        let shown = StorageResponse {
            id: 2,
            name: "cloud".into(),
            path: "/mnt/s3".into(),
            r#type: "s3fs".into(),
            scheme: "Medium".into(),
            server_id: None,
            url: Some("s3fs://AKIAEXAMPLE:wJalrXUtnFEMI@s3.example.com/bucket".into()),
            disk_space: None,
            do_delete: 1,
            enabled: 1,
            filesystem: None,
        }
        .without_secrets();
        let url = shown.url.unwrap();
        assert!(!url.contains("wJalrXUtnFEMI"), "{url}");
        assert!(url.contains("AKIAEXAMPLE") && url.contains("s3.example.com/bucket"));
    }
}
