

select
    date(placed_at) as order_date,
    count(*) as order_count,
    sum(total_amount) as total_revenue,
    avg(total_amount) as avg_order_value
from "AwsDataCatalog"."de_picnic_dev_example_silver"."stg_test_raw_raw_orders"
group by
    date(placed_at)