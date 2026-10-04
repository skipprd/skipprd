{{ config(alias="stg_bronze_shop") }}
SELECT
    CAST(id AS BIGINT) AS id,
    CAST(name AS STRING) AS name,
    CAST(amount AS BIGINT) AS amount,
    id IS NOT NULL AND CAST(id AS BIGINT) IS NOT NULL AS id_quality_flag,
    name IS NOT NULL AND CAST(name AS STRING) IS NOT NULL AS name_quality_flag,
    amount IS NOT NULL AND CAST(amount AS BIGINT) IS NOT NULL AS amount_quality_flag
FROM {{ source("bronze", "shop") }}
