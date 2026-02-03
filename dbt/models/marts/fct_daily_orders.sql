{{ config(schema="gold", alias="fct_daily_orders") }}

select
    date(placed_at) as order_date,
    count(*) as order_count,
    sum(total_amount) as total_revenue,
    avg(total_amount) as avg_order_value
from {{ ref('stg_test_raw_raw_orders') }}
group by
    date(placed_at)