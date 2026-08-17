use std::net::SocketAddr;

use datafusion::arrow::array::RecordBatch;
use skippr_lease::DurableError;
use skippr_query_ballista::fetch_statement_batches;

pub async fn fetch_flight_sql(
    endpoint: SocketAddr,
    sql: &str,
) -> Result<Vec<RecordBatch>, DurableError> {
    match tokio::time::timeout(
        skippr_lease::RPC_IDLE_TIMEOUT,
        fetch_statement_batches(&endpoint.to_string(), sql),
    )
    .await
    {
        Ok(Ok(batches)) => Ok(batches),
        Ok(Err(err)) => Err(DurableError::Io(err.to_string())),
        Err(_) => Err(DurableError::Io("Flight SQL idle timeout".into())),
    }
}
