{{ config(alias="agg_shop_summary") }}
SELECT
    COUNT(*) AS record_count,
    COUNT(DISTINCT shop_id) AS distinct_shop_count,
    SUM(amount) AS amount_sum,
    AVG(amount) AS amount_average,
    COUNT(CASE WHEN amount_quality_flag = TRUE THEN 1 END) AS valid_amount_count
FROM {{ ref("fct_shop") }}
