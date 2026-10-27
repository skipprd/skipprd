pub fn table_name_for_pipeline(pipeline: &str) -> String {
    format!("_dl_{pipeline}")
}

#[cfg(test)]
mod tests {
    use super::table_name_for_pipeline;

    #[test]
    fn deadletter_table_is_pipeline_prefixed() {
        assert_eq!(table_name_for_pipeline("orders"), "_dl_orders");
    }
}
