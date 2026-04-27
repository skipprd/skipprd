use std::collections::BTreeMap;

use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client;
use chrono::{Days, NaiveDate, Utc};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct AccountProfile {
    pub tenant_id: String,
    pub domain: Option<String>,
    pub plan: String,
    pub billing_status: String,
    pub stripe_customer_id: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Balance {
    pub balance: f64,
    pub version: u64,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LedgerEntry {
    pub id: String,
    pub timestamp: String,
    pub entry_type: String,
    pub amount: f64,
    pub balance_after: f64,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DailyCost {
    pub date: String,
    pub cost: f64,
}

fn attr_s(item: &std::collections::HashMap<String, AttributeValue>, key: &str) -> String {
    item.get(key)
        .and_then(|v| v.as_s().ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn attr_n_f64(item: &std::collections::HashMap<String, AttributeValue>, key: &str) -> f64 {
    item.get(key)
        .and_then(|v| v.as_n().ok())
        .and_then(|n| n.parse::<f64>().ok())
        .unwrap_or(0.0)
}

fn attr_n_u64(item: &std::collections::HashMap<String, AttributeValue>, key: &str) -> u64 {
    item.get(key)
        .and_then(|v| v.as_n().ok())
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0)
}

pub async fn get_profile(
    client: &Client,
    table: &str,
    tenant_id: &str,
) -> Result<AccountProfile, String> {
    let result = client
        .get_item()
        .table_name(table)
        .key("PK", AttributeValue::S(format!("TENANT#{tenant_id}")))
        .key("SK", AttributeValue::S("PROFILE".to_string()))
        .send()
        .await
        .map_err(|e| format!("DDB get profile: {e:?}"))?;

    let item = result
        .item()
        .ok_or_else(|| format!("No profile found for tenant '{tenant_id}'"))?;

    Ok(AccountProfile {
        tenant_id: tenant_id.to_string(),
        domain: item
            .get("domain")
            .and_then(|v| v.as_s().ok())
            .map(|s| s.to_string()),
        plan: attr_s(item, "plan"),
        billing_status: attr_s(item, "billing_status"),
        stripe_customer_id: item
            .get("stripe_customer_id")
            .and_then(|v| v.as_s().ok())
            .map(|s| s.to_string()),
        created_at: attr_s(item, "created_at"),
    })
}

pub async fn get_balance(client: &Client, table: &str, tenant_id: &str) -> Result<Balance, String> {
    let result = client
        .get_item()
        .table_name(table)
        .key("PK", AttributeValue::S(format!("TENANT#{tenant_id}")))
        .key("SK", AttributeValue::S("BALANCE".to_string()))
        .consistent_read(true)
        .send()
        .await
        .map_err(|e| format!("DDB get balance: {e:?}"))?;

    let item = result
        .item()
        .ok_or_else(|| format!("No balance found for tenant '{tenant_id}'"))?;

    Ok(Balance {
        balance: attr_n_f64(item, "balance"),
        version: attr_n_u64(item, "version"),
        updated_at: attr_s(item, "updated_at"),
    })
}

pub async fn get_ledger(
    client: &Client,
    table: &str,
    tenant_id: &str,
    limit: i32,
) -> Result<Vec<LedgerEntry>, String> {
    let pk = format!("TENANT#{tenant_id}");

    let result = client
        .query()
        .table_name(table)
        .key_condition_expression("PK = :pk AND begins_with(SK, :prefix)")
        .expression_attribute_values(":pk", AttributeValue::S(pk))
        .expression_attribute_values(":prefix", AttributeValue::S("LEDGER#".to_string()))
        .scan_index_forward(false)
        .limit(limit)
        .send()
        .await
        .map_err(|e| format!("DDB query ledger: {e:?}"))?;

    let entries = result
        .items()
        .iter()
        .filter_map(|item| {
            let sk = attr_s(item, "SK");
            let rest = sk.strip_prefix("LEDGER#")?;
            let (timestamp, id) = match rest.find('#') {
                Some(pos) => (rest[..pos].to_string(), rest[pos + 1..].to_string()),
                None => (rest.to_string(), String::new()),
            };
            Some(LedgerEntry {
                id,
                timestamp,
                entry_type: attr_s(item, "entry_type"),
                amount: attr_n_f64(item, "amount"),
                balance_after: attr_n_f64(item, "balance_after"),
                description: attr_s(item, "description"),
            })
        })
        .collect();

    Ok(entries)
}

pub async fn get_daily_costs_est(
    client: &Client,
    table: &str,
    tenant_id: &str,
    days: u64,
) -> Result<Vec<DailyCost>, String> {
    let today = Utc::now().date_naive();
    let start_date = today.checked_sub_days(Days::new(days - 1)).unwrap_or(today);

    let pk = format!("TENANT#{tenant_id}");
    let sk_start = format!("LEDGER#{}", start_date.format("%Y-%m-%d"));
    let sk_end = "LEDGER#~".to_string();

    let mut totals: BTreeMap<NaiveDate, f64> = BTreeMap::new();
    let mut exclusive_start_key: Option<std::collections::HashMap<String, AttributeValue>> = None;

    loop {
        let mut query = client
            .query()
            .table_name(table)
            .key_condition_expression("PK = :pk AND SK BETWEEN :sk_start AND :sk_end")
            .expression_attribute_values(":pk", AttributeValue::S(pk.clone()))
            .expression_attribute_values(":sk_start", AttributeValue::S(sk_start.clone()))
            .expression_attribute_values(":sk_end", AttributeValue::S(sk_end.clone()))
            .filter_expression("entry_type = :debit")
            .expression_attribute_values(":debit", AttributeValue::S("debit".to_string()))
            .projection_expression("SK, amount");

        if let Some(ref key) = exclusive_start_key {
            query = query.set_exclusive_start_key(Some(key.clone()));
        }

        let result = query
            .send()
            .await
            .map_err(|e| format!("DDB query daily costs: {e:?}"))?;

        for item in result.items() {
            let sk = attr_s(item, "SK");
            if let Some(rest) = sk.strip_prefix("LEDGER#") {
                if let Some(date_str) = rest.get(..10) {
                    if let Ok(date) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
                        let amount = attr_n_f64(item, "amount").abs();
                        *totals.entry(date).or_default() += amount;
                    }
                }
            }
        }

        match result.last_evaluated_key() {
            Some(key) if !key.is_empty() => {
                exclusive_start_key = Some(key.clone());
            }
            _ => break,
        }
    }

    let mut costs = Vec::new();
    let mut d = start_date;
    while d <= today {
        costs.push(DailyCost {
            date: d.format("%Y-%m-%d").to_string(),
            cost: *totals.get(&d).unwrap_or(&0.0),
        });
        d = d.succ_opt().unwrap_or(d);
    }

    Ok(costs)
}
