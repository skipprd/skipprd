create or replace view
    "AwsDataCatalog"."de_picnic_dev_example_gold"."dim_customers"
  as
    

with customers as (

    select
        -- business key
        customer_id,

        -- natural keys / identifiers
        email,

        -- name attributes
        first_name,
        last_name,

        -- lifecycle / signup timestamp
        created_at

    from "AwsDataCatalog"."de_picnic_dev_example_silver"."stg_test_raw_raw_customers"
    where customer_id is not null
)

select
    customer_id,
    email,
    first_name,
    last_name,
    created_at as signup_at
from customers
