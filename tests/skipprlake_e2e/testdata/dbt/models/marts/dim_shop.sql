{{ config(alias="dim_shop") }}
SELECT
    id AS shop_id,
    name AS shop_name,
    id_quality_flag,
    name_quality_flag
FROM {{ ref("stg_bronze_shop") }}
