pub fn intent_extraction(user_q: &str) -> String {
    format!(r#"You are a data analyst. Extract the analysis intent from the Question.
Return strict JSON with keys: entities[], measures[], filters[], time_range{{from,to}}, aggregation.
Question: {}"#, user_q)
}

pub fn candidate_ranking(context: &str, user_q: &str) -> String {
    format!(r#"You are a data model selector. Given Datasets and Fields context (including dataset root descriptions), select relevant datasets and fields for the Question.
Return strict JSON: {{ candidates: [ {{ namespace: string, score: number, fields: string[], rationale: string }} ] }}.
Datasets:
{}
Question: {}"#, context, user_q)
}

pub fn sql_generation(context: &str, join_hints: &str, user_q: &str, top_k: usize) -> String {
    format!(r#"You are a SQL generator. Given Datasets/Fields context and Join hints, write a single SELECT query to answer the Question.
Rules: SELECT only; no DDL/DML; include LIMIT {}; prefer aggregates; qualify columns with table names.
Return ONLY the SQL, no prose.
Context:
{}
Joins:
{}
Question: {}"#, top_k, context, join_hints, user_q)
}

pub fn final_answer(context: &str, sql: &str, data: &str, user_q: &str) -> String {
    format!(r#"You are a data assistant. Using Context, SQL and Data, provide a concise answer to the Question.
Rules: No SQL or code in the answer. You may include a small table if helpful.
Context:
{}
SQL:
{}
Data:
{}
Question: {}"#, context, sql, data, user_q)
}


