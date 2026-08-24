use std::time::{SystemTime, UNIX_EPOCH};

use datafusion::error::DataFusionError;

/// Default lookback when both bounds are omitted (24 hours in nanoseconds).
pub const OTEL_DEFAULT_LOOKBACK: i64 = 24 * 60 * 60 * 1_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanBudgetError {
    PartialWindow,
    InvertedWindow,
    RangeTooWide,
}

impl std::fmt::Display for ScanBudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PartialWindow => write!(f, "ScanBudget requires both from and to, or neither"),
            Self::InvertedWindow => write!(f, "ScanBudget from must be <= to"),
            Self::RangeTooWide => {
                write!(
                    f,
                    "RangeTooWide: ScanBudget window exceeds {}ns ceiling",
                    OTEL_DEFAULT_LOOKBACK
                )
            }
        }
    }
}

impl std::error::Error for ScanBudgetError {}

impl From<ScanBudgetError> for DataFusionError {
    fn from(err: ScanBudgetError) -> Self {
        DataFusionError::Plan(err.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantFilter {
    pub tenant_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanBudget {
    pub from_unix_nano: i64,
    pub to_unix_nano: i64,
    pub tenant: TenantFilter,
}

impl ScanBudget {
    pub fn now_unix_nano() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0)
    }

    pub fn new(
        from_unix_nano: Option<i64>,
        to_unix_nano: Option<i64>,
        tenant_id: Option<String>,
    ) -> Result<Self, ScanBudgetError> {
        Self::new_at(
            from_unix_nano,
            to_unix_nano,
            tenant_id,
            Self::now_unix_nano(),
        )
    }

    pub fn new_at(
        from_unix_nano: Option<i64>,
        to_unix_nano: Option<i64>,
        tenant_id: Option<String>,
        now_unix_nano: i64,
    ) -> Result<Self, ScanBudgetError> {
        let (from_unix_nano, to_unix_nano) = match (from_unix_nano, to_unix_nano) {
            (None, None) => (
                now_unix_nano.saturating_sub(OTEL_DEFAULT_LOOKBACK),
                now_unix_nano,
            ),
            (Some(_), None) | (None, Some(_)) => return Err(ScanBudgetError::PartialWindow),
            (Some(from), Some(to)) if from > to => return Err(ScanBudgetError::InvertedWindow),
            (Some(from), Some(to)) => (from, to),
        };
        if to_unix_nano.saturating_sub(from_unix_nano) > OTEL_DEFAULT_LOOKBACK {
            return Err(ScanBudgetError::RangeTooWide);
        }
        Ok(Self {
            from_unix_nano,
            to_unix_nano,
            tenant: TenantFilter { tenant_id },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_lookback_when_both_omitted() {
        let budget = ScanBudget::new_at(None, None, None, 2_000_000_000_000).unwrap();
        assert_eq!(
            budget.to_unix_nano - budget.from_unix_nano,
            OTEL_DEFAULT_LOOKBACK
        );
    }

    #[test]
    fn partial_window_fails_closed() {
        assert_eq!(
            ScanBudget::new_at(Some(1), None, None, 10).unwrap_err(),
            ScanBudgetError::PartialWindow
        );
    }

    #[test]
    fn inverted_window_fails_closed() {
        assert_eq!(
            ScanBudget::new_at(Some(5), Some(1), None, 10).unwrap_err(),
            ScanBudgetError::InvertedWindow
        );
    }

    #[test]
    fn window_above_ceiling_fails_closed() {
        let from = 0;
        let to = OTEL_DEFAULT_LOOKBACK + 1;
        assert_eq!(
            ScanBudget::new_at(Some(from), Some(to), None, to).unwrap_err(),
            ScanBudgetError::RangeTooWide
        );
    }
}
