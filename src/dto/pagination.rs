//! Common pagination types for list endpoints

use garde::Validate;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Default page size when not specified
pub const DEFAULT_PAGE_SIZE: u64 = 25;

/// Maximum page size allowed
pub const MAX_PAGE_SIZE: u64 = 1000;

/// The highest page number honoured. A row offset is `page * page_size`, here
/// and inside the ORM's paginator; with a page near `u64::MAX` that multiply
/// overflows, which panics a debug build and wraps to an arbitrary offset in
/// a release one. No table has this many pages.
pub const MAX_PAGE: u64 = u32::MAX as u64;

/// Common pagination query parameters for list endpoints
#[derive(Debug, Clone, Default, Deserialize, Serialize, ToSchema, Validate)]
pub struct PaginationParams {
    /// Page number (1-indexed, defaults to 1)
    #[schema(example = 1, minimum = 1)]
    #[garde(range(min = 1))]
    pub page: Option<u64>,

    /// Number of items per page (defaults to 25, max 1000)
    #[schema(example = 25, minimum = 1, maximum = 1000)]
    #[garde(range(min = 1, max = 1000))]
    pub page_size: Option<u64>,
}

impl PaginationParams {
    /// Get the page number (1-indexed), defaulting to 1
    pub fn page(&self) -> u64 {
        self.page.unwrap_or(1).clamp(1, MAX_PAGE)
    }

    /// Get the page size, defaulting to DEFAULT_PAGE_SIZE
    pub fn page_size(&self) -> u64 {
        self.page_size
            .unwrap_or(DEFAULT_PAGE_SIZE)
            .clamp(1, MAX_PAGE_SIZE)
    }

    /// Get the offset for database queries
    pub fn offset(&self) -> u64 {
        (self.page().saturating_sub(1)) * self.page_size()
    }

    /// Get the limit for database queries
    pub fn limit(&self) -> u64 {
        self.page_size()
    }
}

/// Generic paginated response wrapper
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PaginatedResponse<T> {
    /// The items for the current page
    pub items: Vec<T>,
    /// Total number of items across all pages
    pub total: u64,
    /// Number of items per page
    pub per_page: u64,
    /// Current page number (1-indexed)
    pub current_page: u64,
    /// Last page number
    pub last_page: u64,
}

impl<T> PaginatedResponse<T> {
    /// Create a new paginated response
    pub fn new(items: Vec<T>, total: u64, page: u64, page_size: u64) -> Self {
        let last_page = if total == 0 {
            1
        } else {
            total.div_ceil(page_size)
        };

        Self {
            items,
            total,
            per_page: page_size,
            current_page: page,
            last_page,
        }
    }

    /// Create from pagination params
    pub fn from_params(items: Vec<T>, total: u64, params: &PaginationParams) -> Self {
        Self::new(items, total, params.page(), params.page_size())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `?page=18446744073709551615` must not overflow the offset.
    #[test]
    fn a_huge_page_number_does_not_overflow_the_offset() {
        let params = PaginationParams {
            page: Some(u64::MAX),
            page_size: Some(MAX_PAGE_SIZE),
        };
        assert_eq!(params.page(), MAX_PAGE);
        assert_eq!(params.offset(), (MAX_PAGE - 1) * MAX_PAGE_SIZE);
        // What the ORM's paginator computes from the page it is given.
        assert!(params.page().checked_mul(params.page_size()).is_some());
    }

    #[test]
    fn test_pagination_params_defaults() {
        let params = PaginationParams::default();
        assert_eq!(params.page(), 1);
        assert_eq!(params.page_size(), DEFAULT_PAGE_SIZE);
        assert_eq!(params.offset(), 0);
        assert_eq!(params.limit(), DEFAULT_PAGE_SIZE);
    }

    #[test]
    fn test_pagination_params_custom() {
        let params = PaginationParams {
            page: Some(3),
            page_size: Some(50),
        };
        assert_eq!(params.page(), 3);
        assert_eq!(params.page_size(), 50);
        assert_eq!(params.offset(), 100); // (3-1) * 50
        assert_eq!(params.limit(), 50);
    }

    #[test]
    fn test_pagination_params_max_page_size() {
        let params = PaginationParams {
            page: Some(1),
            page_size: Some(5000), // Over max
        };
        assert_eq!(params.page_size(), MAX_PAGE_SIZE);
    }

    #[test]
    fn test_paginated_response() {
        let items = vec![1, 2, 3];
        let response = PaginatedResponse::new(items, 100, 2, 25);
        assert_eq!(response.total, 100);
        assert_eq!(response.per_page, 25);
        assert_eq!(response.current_page, 2);
        assert_eq!(response.last_page, 4); // 100 / 25 = 4
    }

    #[test]
    fn test_paginated_response_empty() {
        let items: Vec<i32> = vec![];
        let response = PaginatedResponse::new(items, 0, 1, 25);
        assert_eq!(response.last_page, 1);
    }
}
