

with order_items as (

    select
        order_item_id,
        order_id,
        product_sku,
        quantity,
        unit_price
    from "AwsDataCatalog"."de_picnic_dev_example_silver"."stg_test_raw_raw_order_items"

), orders as (

    select
        order_id,
        customer_id,
        order_status,
        total_amount,
        placed_at
    from "AwsDataCatalog"."de_picnic_dev_example_silver"."stg_test_raw_raw_orders"

), joined as (

    select
        -- grain: one row per order item
        oi.order_item_id,

        -- order / customer context
        oi.order_id,
        o.customer_id,

        -- product
        oi.product_sku,

        -- measures at item grain
        oi.quantity,
        oi.unit_price,
        (oi.quantity * oi.unit_price) as line_item_revenue,

        -- order-level context fields
        o.total_amount           as order_total_amount,
        o.order_status,
        o.placed_at

    from order_items oi
    left join orders o
        on oi.order_id = o.order_id
)

select *
from joined