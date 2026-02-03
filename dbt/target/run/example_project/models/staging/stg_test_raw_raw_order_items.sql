create or replace view
    "AwsDataCatalog"."de_picnic_dev_example_silver"."stg_test_raw_raw_order_items"
  as
    

with source as (

    select
        order_item_id,
        order_id,
        product_sku,
        quantity,
        unit_price

    from "AwsDataCatalog"."test_raw"."raw_order_items"
),

renamed as (

    select
        -- business keys / identifiers
        trim(nullif(order_item_id, ''))               as order_item_id,
        trim(nullif(order_id, ''))                    as order_id,

        -- product
        trim(nullif(product_sku, ''))                 as product_sku,

        -- numeric fields (stored as strings)
        try_cast(trim(nullif(quantity, '')) as bigint)    as quantity,
        try_cast(trim(nullif(unit_price, '')) as double)  as unit_price

    from source
)

select *
from renamed
