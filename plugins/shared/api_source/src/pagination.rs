#[derive(Clone, Debug, Default)]
pub struct PageNumberPagination {
    pub page: u32,
    pub page_size: u32,
}

impl PageNumberPagination {
    pub fn next_page(&mut self) -> u32 {
        let current = self.page;
        self.page += 1;
        current
    }

    pub fn should_stop(&self, rows_returned: usize) -> bool {
        rows_returned < self.page_size as usize
    }
}

#[derive(Clone, Debug, Default)]
pub struct OffsetPagination {
    pub offset: u64,
    pub limit: u64,
}

impl OffsetPagination {
    pub fn advance(&mut self, rows_returned: usize) {
        self.offset += rows_returned as u64;
    }

    pub fn should_stop(&self, rows_returned: usize) -> bool {
        rows_returned < self.limit as usize
    }
}

#[derive(Clone, Debug, Default)]
pub struct TokenPagination {
    pub next_token: Option<String>,
}

impl TokenPagination {
    pub fn should_continue(&self) -> bool {
        self.next_token
            .as_ref()
            .is_some_and(|t| !t.trim().is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_number_stops_on_short_page() {
        let pag = PageNumberPagination {
            page: 1,
            page_size: 100,
        };
        assert!(pag.should_stop(50));
        assert!(!pag.should_stop(100));
    }

    #[test]
    fn token_pagination_requires_token() {
        let mut pag = TokenPagination::default();
        assert!(!pag.should_continue());
        pag.next_token = Some("abc".into());
        assert!(pag.should_continue());
    }
}
