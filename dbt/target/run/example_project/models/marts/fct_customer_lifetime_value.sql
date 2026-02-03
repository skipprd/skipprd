create or replace view
    "AwsDataCatalog"."de_picnic_dev_example_gold"."fct_customer_lifetime_value"
  as
    

with orders as (

    select
        order_id,
        customer_id,
        order_status,
        total_amount,
        placed_at
    from "AwsDataCatalog"."de_picnic_dev_example_silver"."stg_test_raw_raw_orders"

), customers as (

    select
        customer_id,
        email,
        first_name,
        last_name,
        created_at
    from "AwsDataCatalog"."de_picnic_dev_example_silver"."stg_test_raw_raw_customers"

), customer_orders as (

    select
        c.customer_id,
        c.email,
        c.first_name,
        c.last_name,
        c.created_at,
        o.order_id,
        o.order_status,
        o.total_amount,
        o.placed_at
    from customers c
    left join orders o
        on c.customer_id = o.customer_id

), aggregated as (

    select
        customer_id,
        email,
        first_name,
        last_name,
        created_at,
        count(distinct order_id)  as order_count,
        sum(total_amount)         as lifetime_revenue,
        min(placed_at)            as first_order_at,
        max(placed_at)            as most_recent_order_at
    from customer_orders
    group by
        customer_id,
        email,
        first_name,
        last_name,
        created_at

)

select
    customer_id,
    email,
    first_name,
    last_name,
    created_at,
    order_count,
    lifetime_revenue,
    first_order_at,
    most_recent_order_at
from aggregated
