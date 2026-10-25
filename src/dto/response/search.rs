//! Response DTOs for the natural-language / semantic event search endpoints.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::service::search::store::Hit;

/// One retrieved event in a search response — the verifiable ground truth.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SearchCitation {
    pub event_id: u64,
    /// Event time, epoch seconds.
    pub ts: i64,
    /// Relevance score (reranker score when reranking is on, else the RRF score;
    /// higher is better).
    pub score: f32,
    /// Snippet of the indexed description text.
    pub snippet: String,
    /// API path to the full event.
    pub url: String,
}

impl From<Hit> for SearchCitation {
    fn from(h: Hit) -> Self {
        let url = crate::configure::server::public_path(&format!("/api/v3/events/{}", h.event_id));
        Self {
            event_id: h.event_id,
            ts: h.ts,
            score: h.score,
            snippet: h.snippet,
            url,
        }
    }
}

/// Response for `GET /api/v3/search`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SearchResponse {
    /// Grounded NL answer when the router LLM is reachable; else `null` and the
    /// client renders the citations directly.
    pub answer: Option<String>,
    /// Exact count from the SQL count tool for "how many …" queries; else `null`.
    pub count: Option<u64>,
    /// The retrieved events.
    pub citations: Vec<SearchCitation>,
    /// Resolved vector backend (diagnostic): `mariadb` | `sqlite` | `none`.
    pub backend: String,
}

/// Response for `GET /api/v3/events/{id}/similar`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SimilarResponse {
    /// Events nearest the source event's embedding.
    pub citations: Vec<SearchCitation>,
    /// Resolved vector backend (diagnostic).
    pub backend: String,
}
