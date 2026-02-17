# ThreadStateSnapshot

## Properties

Name | Type | Description | Notes
------------ | ------------- | ------------- | -------------
**thread_state_schema_version** | **i32** |  | 
**thread_id** | **String** |  | 
**suite_id** | Option<**String**> |  | [optional]
**agent_type** | Option<**String**> |  | [optional]
**current_phase** | Option<**String**> |  | [optional]
**last_materialized_step_count** | **i32** |  | 
**total_runtime_ms** | **i64** |  | 
**phases** | **Vec<String>** | Ordered suite phases for the current suite+agent_type (suite-owned ordering). Clients should treat this as the authoritative phase order for rendering progress.  | 
**completed_phases** | **Vec<String>** | Subset of `phases` that are considered completed, derived from `currentPhase` and the suite's phase order. (Does not include the current in-flight phase.)  | 
**events** | [**Vec<models::ThreadEvent>**](ThreadEvent.md) | Recent, durable timeline events for UI. This is a bounded, best-effort list intended for \"post reconnect\" tool timelines. (Not a complete event log.)  | 
**items** | [**Vec<models::ThreadStateItem>**](ThreadStateItem.md) |  | 
**plan_summaries** | Option<[**std::collections::HashMap<String, serde_json::Value>**](serde_json::Value.md)> | Summary-only plan refs (opaque JSON by kind). | [optional]

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)


