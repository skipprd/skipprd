use chrono::NaiveDate;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DateWindow {
    pub start: NaiveDate,
    pub end: NaiveDate,
}

#[derive(Clone, Debug)]
pub struct DateWindowPlanner {
    pub lookback_days: u32,
}

impl DateWindowPlanner {
    pub fn plan(
        &self,
        start_date: NaiveDate,
        last_completed: Option<NaiveDate>,
        end_date: NaiveDate,
    ) -> DateWindow {
        let refresh_start = last_completed
            .map(|d| d - chrono::Duration::days(self.lookback_days as i64))
            .unwrap_or(start_date);
        let effective_start = refresh_start.max(start_date);
        DateWindow {
            start: effective_start,
            end: end_date,
        }
    }

    pub fn dates_inclusive(window: &DateWindow) -> Vec<NaiveDate> {
        let mut dates = Vec::new();
        let mut cursor = window.start;
        while cursor <= window.end {
            dates.push(cursor);
            cursor += chrono::Duration::days(1);
        }
        dates
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookback_overlaps_prior_checkpoint() {
        let planner = DateWindowPlanner { lookback_days: 3 };
        let window = planner.plan(
            NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
            Some(NaiveDate::from_ymd_opt(2024, 1, 10).unwrap()),
            NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
        );
        assert_eq!(window.start, NaiveDate::from_ymd_opt(2024, 1, 7).unwrap());
        assert_eq!(window.end, NaiveDate::from_ymd_opt(2024, 1, 15).unwrap());
    }

    #[test]
    fn inclusive_dates_count() {
        let window = DateWindow {
            start: NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
            end: NaiveDate::from_ymd_opt(2024, 1, 3).unwrap(),
        };
        assert_eq!(DateWindowPlanner::dates_inclusive(&window).len(), 3);
    }
}
