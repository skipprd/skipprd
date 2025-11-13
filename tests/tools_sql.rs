use datafusion::prelude::*;

#[tokio::test]
#[ignore]
async fn test_sql_run_enforces_limit() {
    let ctx = SessionContext::new();
    let df = ctx.sql("SELECT 1 AS x").await.unwrap();
    ctx.register_table("t", df.into_view()).unwrap();
    let res = ctx.sql("SELECT x FROM t LIMIT 1").await.unwrap().collect().await.unwrap();
    assert_eq!(res.len(), 1);
}

#[tokio::test]
#[ignore]
async fn test_sql_schema_describe_table() {
    let ctx = SessionContext::new();
    let df = ctx.sql("SELECT 1 AS x, 'a' AS y").await.unwrap();
    ctx.register_table("t2", df.into_view()).unwrap();
    let df = ctx.sql("SELECT * FROM t2").await.unwrap();
    let batches = df.collect().await.unwrap();
    assert_eq!(batches[0].schema().fields().len(), 2);
}


