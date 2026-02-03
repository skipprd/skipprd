{{ config(schema="gold", alias="fct_orders") }}

with orders as (

    select
        order_id,
        customer_id,
        order_status,
        total_amount,
        placed_at
    from {{ ref('stg_test_raw_raw_orders') }}

), customers as (

    select
        customer_id,
        email,
        first_name,
        last_name,
        created_at
    from {{ ref('stg_test_raw_raw_customers') }}

), joined as (

    select
        o.order_id,
        o.customer_id,
        o.order_status,
        o.total_amount,
        o.placed_at,
        c.email as customer_email,
        c.first_name as customer_first_name,
        c.last_name as customer_last_name,
        c.created_at as customer_created_at
    from orders o
    left join customers c
        on o.customer_id = c.customer_id

)

select
    order_id,
    customer_id,
    order_status,
    total_amount,
    placed_at,
    customer_email,
    customer_first_name,
    customer_last_name,
    customer_created_at
from joined