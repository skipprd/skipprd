{{ config(alias="fct_shop") }}
SELECT
    id AS shop_id,
    name AS shop_name,
    amount,
    id_quality_flag,
    name_quality_flag,
    amount_quality_flag
FROM {{ ref("stg_bronze_shop") }}
