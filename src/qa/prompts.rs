pub fn system_prompt() -> String {
    r#"You are a SQL/data agent. At each step, you must either:
- Call ONE tool (return STRICT JSON: {"action": "<tool_name>", "args": {...}})
- Or finish with STRICT JSON: {"final": {"sql": "<SELECT ...>", "answer": "<concise>"}}
Rules: STRICT JSON only. No prose. SELECT-only SQL with LIMIT. Avoid timestamp range()/generate_series; prefer date_trunc for time buckets.
CRITICAL: Always reference tables as <pipeline>.<namespace> (e.g., picnic.screen). Never use unqualified table names."#.to_string()
}

pub fn tool_card() -> String {
    r#"Tools:
- vect_query(args:{scope:"dataset"|"field"|"doc", query_text:string, k:int}) -> {"ok":true,"items":[{"kind":string,"namespace":string,"field":string?,"text":string,"score":float}]}
- sql_schema(args:{table?:string}) -> {"ok":true,"tables":[...]} or {"ok":true,"columns":[{"name":string,"type":string}]} (tables should be fully-qualified when known)
- sql_stats(args:{table:string, field:string}) -> {"ok":true,"stats":{"distinct":int?,"min":float?,"max":float?,"max_len":int?,"nulls":int}}
- sql_sample(args:{table:string, field:string, k:int}) -> {"ok":true,"values":[{"value":string,"count":int}]}
- run_sql(args:{sql:string}) -> {"ok":true,"header":[string], "rows":[[string]]} or {"ok":false,"error":string}"#.to_string()
}

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
ALL table references MUST be fully-qualified as <pipeline>.<namespace>. Do NOT use unqualified tables.
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

pub fn dataset_selection(candidates_ctx: &str, user_q: &str) -> String {
    format!(r#"You are a data model selector.
Given the Candidate datasets (with descriptions and field samples) and the Question, choose the single most relevant dataset.
Respond with STRICT JSON: {{"namespace": string, "rationale": string}}.
If unsure, still choose the best available namespace.

Candidates:
{}

Question: {}
Output JSON:"#, candidates_ctx, user_q)
}

pub fn field_selection(fields_ctx: &str, schema_ctx: &str, user_q: &str, top_k: usize) -> String {
    format!(r#"You are a field selector.
Choose the best grouping field to answer the Question using the fields available in the dataset and actual schema types.
Optionally choose a time field if it will help provide useful context. Do not invent fields.
Respond with STRICT JSON: {{"groupField": string, "timeField": string | null, "filters": string[]}}.
Important:
- The schema list shows TOP-LEVEL columns only. Nested fields may be referenced using dotted paths (e.g., hardware.model) if present in the catalog fields.
- Therefore, groupField/timeField MUST exist in the catalog fields; they MAY be dotted and not appear verbatim in the schema list, as long as their top-level segment exists in the schema.
- Prefer user-identifying Id fields (e.g., profile_id, user_id, userid, account_id) when the Question is about users, accounts, or DAU/MAU/retention.
- filters may be empty.

Fields (from catalog):
{}

Schema (top-level name:type):
{}

Question: {}
TopK: {}
Output JSON:"#, fields_ctx, schema_ctx, user_q, top_k)
}

pub fn sql_generation_json(namespace: &str, user_q: &str, stats_ctx: &str, top_k: usize) -> String {
    format!(r#"You are a SQL generator.
Write a single SELECT query to answer the Question using the provided dataset and fields.
Rules:
- SELECT only; no DDL/DML
- Use dataset (fully-qualified): {ns}
- ALL table references MUST be fully-qualified as <pipeline>.<namespace>
- Prefer simple, robust aggregations. If grouping is needed, choose an appropriate grouping column based on the Question (e.g., a user/account/profile identifier for user questions).
- Return at most {k} rows using LIMIT {k}
- Do not reference columns that do not exist
- DataFusion constraints: do NOT use range() or generate_series() with timestamps. For daily/weekly results, use date_trunc('day'|'week', timeField) and GROUP BY that.
- Prefer date_trunc-based grouping over synthetic date series. If you must generate a series, use Int64 range and cast with to_timestamp_millis(), but avoid unless explicitly asked.
Output formatting requirements:
- The SQL MUST be a single SELECT statement and MUST start with the word SELECT
- No CTEs unless strictly necessary; no procedural constructs
- Do NOT include markdown, code fences, or prose
- Respond with STRICT JSON only: {{"sql": "<YOUR SQL HERE>"}}

Inputs:
dataset: {ns}
Stats (approx): {stats}

Question: {q}
Output JSON:"#,
        ns = namespace,
        k = top_k,
        stats = stats_ctx,
        // gf = group_field,
        // tf = tf,
        q = user_q)

    // groupField: {gf}
    // timeField: {tf}
}

pub fn sql_repair(previous_json: &str, error_text: &str, schema_ctx: &str) -> String {
    format!(r#"You are a SQL fixer.
The previous SQL failed with an error. Produce a corrected SQL.
Rules:
- Keep intent and structure, but fix column names, qualifiers, or syntax
- Use only columns that exist in the provided schema
- The SQL MUST be a single SELECT statement and MUST start with the word SELECT
- Do NOT include markdown, code fences, or prose
- Respond with STRICT JSON only: {{"sql": string}}

Previous JSON:
{}

Error:
{}

Schema (name:type):
{}

Output JSON:"#, previous_json, error_text, schema_ctx)
}

pub fn english_synthesis(meta_ctx: &str, sql_json: &str, rows_ctx: &str, extra_ctx: &str, user_q: &str) -> String {
    format!(r#"You are a data assistant.
Given the Question, SQL (JSON), result rows, and optional extra context, produce ONE concise English sentence that directly answers the Question.
You may add obvious, helpful context (e.g., totals or since-earliest), but only if it improves clarity. Avoid jargon. No SQL/code in the answer.

Context:
{}

SQL (JSON):
{}

Rows (CSV-like, header on first line):
{}

Extra:
{}

Question: {}
Answer:"#, meta_ctx, sql_json, rows_ctx, extra_ctx, user_q)
}

pub fn field_selection_with_names(fields_ctx: &str, schema_ctx: &str, field_names_json: &str, user_q: &str, top_k: usize) -> String {
    format!(r#"You are a field selector.
Choose the best grouping field to answer the Question using the dataset's catalog fields and actual schema types.
Also optionally choose a time field if it adds useful context. Do not invent fields.
Respond with STRICT JSON: {{"groupField": string, "timeField": string | null, "filters": string[]}}.
Rules:
- You MUST pick groupField from the provided candidate field names array exactly.
- Prefer user-identifying Id fields (e.g., profile_id, user_id, userid, account_id) when the Question is about users, accounts, or DAU/MAU/retention.
- Candidate field names may include dotted nested paths (e.g., hardware.model).
- The schema list shows only top-level columns; dotted paths are valid if their first segment is in the schema.
- filters may be empty.

Candidate field names (JSON array):
{}

Fields detail (from catalog):
{}

Schema (top-level name:type):
{}

Question: {}
TopK: {}
Output JSON:"#, field_names_json, fields_ctx, schema_ctx, user_q, top_k)
}



