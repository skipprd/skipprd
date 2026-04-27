#[cfg(test)]
pub fn dbt_model_example() -> &'static str {
    r#"{{ config(
    materialized='table',
    tags=['analytics', 'daily']
) }}

with user_activity as (
    select
        user_id,
        date_trunc('day', event_ts) as activity_date
    from {{ source('picnic','track') }}
    where event_type = 'login'
),

daily_active_users as (
    select
        activity_date,
        count(distinct user_id) as dau
    from user_activity
    group by activity_date
)

select *
from daily_active_users
order by activity_date
"#
}

#[cfg(test)]
pub fn metricflow_example() -> &'static str {
    r#"version: 1

entities:
  - name: user
    type: primary
    expr: user_id

dimensions:
  - name: activity_date
    type: time
    type_params:
      time_granularity: day
    expr: date_trunc('day', event_ts)

measures:
  - name: dau
    agg: count_distinct
    expr: user_id

metrics:
  - name: daily_active_users
    type: metric
    description: Daily count of unique active users
    type_params:
      measure: dau
    dimensions:
      - activity_date
"#
}
