{{ config(schema="silver", alias="stg_test_raw_raw_orders") }}

with source as (

    select
        order_id,
        customer_id,
        order_status,
        nullif(trim(total_amount), '') as total_amount_raw,
        try_cast(nullif(trim(total_amount), '') as double) as total_amount,
        nullif(trim(placed_at), '') as placed_at_raw,
        try_cast(nullif(trim(placed_at), '') as timestamp) as placed_at

    from {{ source("test_raw", "raw_orders") }}

),

renamed as (

    select
        order_id,
        customer_id,
        order_status,
        total_amount_raw,
        total_amount,
        placed_at_raw,
        placed_at

    from source
)

select *
from renamed