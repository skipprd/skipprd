use serde_json::{json, Value};

/// Build an allintitle_daily row from a Bright Data `general.results_cnt` value.
pub fn build_allintitle_row(
    site: &str,
    run_date: &str,
    keyword: &str,
    allintitle_count: Option<u64>,
    country: &str,
    language: &str,
    device: &str,
) -> Value {
    let query_status = if allintitle_count.is_some() {
        "ok"
    } else {
        "error"
    };
    let allintitle_count = allintitle_count.unwrap_or(0);
    let kgr = 0.0;
    let kgr_bucket = kgr_bucket_label(kgr);
    let exact_title_competition_score = exact_title_competition_score(allintitle_count);
    json!({
        "site": site,
        "run_date": run_date,
        "keyword": keyword,
        "allintitle_count": allintitle_count,
        "search_volume": 0,
        "kgr": kgr,
        "kgr_bucket": kgr_bucket,
        "exact_title_competition_score": exact_title_competition_score,
        "query_status": query_status,
        "country": country,
        "language": language,
        "device": device,
        "fetch_backend": "brightdata",
    })
}

pub fn kgr_bucket_label(kgr: f64) -> &'static str {
    if kgr <= 0.25 {
        "golden"
    } else if kgr <= 1.0 {
        "good"
    } else if kgr <= 2.0 {
        "moderate"
    } else {
        "hard"
    }
}

pub fn exact_title_competition_score(allintitle_count: u64) -> f64 {
    if allintitle_count == 0 {
        0.0
    } else if allintitle_count <= 10 {
        0.1
    } else if allintitle_count <= 50 {
        0.35
    } else if allintitle_count <= 250 {
        0.65
    } else {
        0.9
    }
}

pub fn compute_kgr(allintitle_count: u64, search_volume: u64) -> f64 {
    if search_volume > 0 {
        allintitle_count as f64 / search_volume as f64
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_kgr_bucket() {
        let row = build_allintitle_row(
            "example.com",
            "2026-06-24",
            "meal planning app free",
            Some(42),
            "gb",
            "en",
            "desktop",
        );
        assert_eq!(row["allintitle_count"], 42);
        assert_eq!(row["kgr_bucket"], "golden");
        assert_eq!(row["query_status"], "ok");
    }

    #[test]
    fn missing_count_marks_error() {
        let row = build_allintitle_row(
            "example.com",
            "2026-06-24",
            "missing",
            None,
            "gb",
            "en",
            "desktop",
        );
        assert_eq!(row["query_status"], "error");
        assert_eq!(row["allintitle_count"], 0);
    }

    #[test]
    fn compute_kgr_with_volume() {
        assert!((compute_kgr(42, 2400) - 0.0175).abs() < 0.0001);
        assert_eq!(compute_kgr(10, 0), 0.0);
    }
}
