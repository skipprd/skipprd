# PlanSnapshot

## Properties

Name | Type | Description | Notes
------------ | ------------- | ------------- | -------------
**plan_kind** | **String** | Suite-defined plan kind token. | 
**plan_key** | **String** |  | 
**status** | [**models::PlanStatus**](PlanStatus.md) |  | 
**tasks** | [**Vec<models::PlanTask>**](PlanTask.md) |  | 
**work_groups** | [**Vec<models::PlanWorkGroup>**](PlanWorkGroup.md) | Ordered, suite-authored work groups for this plan. Each group references one or more checklist items (by taskId + checklistItemId) so clients can render a coherent work plan and the server can deterministically execute the next incomplete group.  | 
**project_snapshot** | Option<[**std::collections::HashMap<String, serde_json::Value>**](serde_json::Value.md)> | Optional, suite-owned snapshot payload for UI/debugging (e.g. batched review notes). This is an opaque JSON object; clients must treat it as best-effort and forward-compatible.  | [optional]

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)


