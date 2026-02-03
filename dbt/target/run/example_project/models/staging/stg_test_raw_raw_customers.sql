create or replace view
    "AwsDataCatalog"."de_picnic_dev_example_silver"."stg_test_raw_raw_customers"
  as
    

with source as (

    select
        customer_id,
        email,
        first_name,
        last_name,
        created_at

    from "AwsDataCatalog"."test_raw"."raw_customers"
),

cleaned as (

    select
        -- business key
        nullif(trim(customer_id), '') as customer_id,

        -- contact info
        nullif(trim(email), '') as email,

        -- name fields
        nullif(trim(first_name), '') as first_name,
        nullif(trim(last_name), '') as last_name,

        -- timestamps
        nullif(trim(created_at), '') as created_at_raw,
        try_cast(nullif(trim(created_at), '') as timestamp) as created_at

    from source
)

select *
from cleaned
