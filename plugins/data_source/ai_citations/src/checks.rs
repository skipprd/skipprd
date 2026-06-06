use serde_json::{json, Value};

pub const CHECK_RESPONSE_SUCCESS: &str = "RESPONSE_SUCCESS";
pub const CHECK_BRAND_MENTIONED: &str = "BRAND_MENTIONED";
pub const CHECK_TARGET_DOMAIN_LINKED: &str = "TARGET_DOMAIN_LINKED";

pub fn map_checks(
    site: &str,
    prompt_id: &str,
    model: &str,
    run_date: &str,
    ok: bool,
    brand_mentioned: bool,
    target_domain_linked: bool,
    error_message: Option<&str>,
) -> Vec<Value> {
    let mut rows = Vec::new();
    rows.push(check_row(
        site,
        prompt_id,
        model,
        run_date,
        CHECK_RESPONSE_SUCCESS,
        if ok { "pass" } else { "fail" },
        if ok { "info" } else { "error" },
        if ok {
            "model response retrieved"
        } else {
            error_message.unwrap_or("model request failed")
        },
    ));
    rows.push(check_row(
        site,
        prompt_id,
        model,
        run_date,
        CHECK_BRAND_MENTIONED,
        if brand_mentioned { "pass" } else { "fail" },
        if brand_mentioned { "info" } else { "warning" },
        if brand_mentioned {
            "brand or alias mentioned in answer"
        } else {
            "brand not mentioned in answer"
        },
    ));
    rows.push(check_row(
        site,
        prompt_id,
        model,
        run_date,
        CHECK_TARGET_DOMAIN_LINKED,
        if target_domain_linked { "pass" } else { "fail" },
        if target_domain_linked {
            "info"
        } else {
            "warning"
        },
        if target_domain_linked {
            "target site domain linked in answer"
        } else {
            "target site domain not linked"
        },
    ));
    rows
}

fn check_row(
    site: &str,
    prompt_id: &str,
    model: &str,
    run_date: &str,
    check_code: &str,
    status: &str,
    severity: &str,
    message: &str,
) -> Value {
    json!({
        "site": site,
        "prompt_id": prompt_id,
        "model": model,
        "run_date": run_date,
        "check_code": check_code,
        "status": status,
        "severity": severity,
        "message": message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_checks_pass_when_response_ok_and_brand_visible() {
        let rows = map_checks(
            "https://example.com",
            "p1",
            "gpt-4.1-mini",
            "2026-05-30",
            true,
            true,
            true,
            None,
        );
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r["status"] == "pass"));
    }

    #[test]
    fn response_check_fails_on_api_error() {
        let rows = map_checks(
            "https://example.com",
            "p1",
            "gpt-4.1-mini",
            "2026-05-30",
            false,
            false,
            false,
            Some("rate limited"),
        );
        let success = rows
            .iter()
            .find(|r| r["check_code"] == CHECK_RESPONSE_SUCCESS)
            .unwrap();
        assert_eq!(success["status"], "fail");
        assert_eq!(success["message"], "rate limited");
    }

    #[test]
    fn brand_check_fails_when_not_mentioned() {
        let rows = map_checks(
            "https://example.com",
            "p1",
            "gpt-4.1-mini",
            "2026-05-30",
            true,
            false,
            false,
            None,
        );
        let brand = rows
            .iter()
            .find(|r| r["check_code"] == CHECK_BRAND_MENTIONED)
            .unwrap();
        assert_eq!(brand["status"], "fail");
        assert_eq!(brand["severity"], "warning");
    }
}
