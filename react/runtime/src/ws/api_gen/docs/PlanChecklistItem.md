# PlanChecklistItem

## Properties

Name | Type | Description | Notes
------------ | ------------- | ------------- | -------------
**checklist_item_id** | **String** | Stable identifier within the task (e.g. sql_model, schema_contract, validate). | 
**label** | **String** | Short UI label. | 
**details** | Option<**String**> | Optional longer-form instructions/details. | [optional]
**status** | [**models::PlanChecklistItemStatus**](PlanChecklistItemStatus.md) |  | 
**origin** | [**models::PlanChecklistOrigin**](PlanChecklistOrigin.md) |  | 
**origin_step_idx** | Option<**i32**> | Thread step index that introduced this requirement (null for initial items). | [optional]
**evidence** | Option<[**Vec<models::PlanChecklistEvidence>**](PlanChecklistEvidence.md)> |  | [optional]

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)


