# ThreadEvent

## Properties

Name | Type | Description | Notes
------------ | ------------- | ------------- | -------------
**step_idx** | **i32** | Index of the backing ThreadStep in the thread log. | 
**event_kind** | [**models::ThreadEventKind**](ThreadEventKind.md) |  | 
**ts** | **String** | RFC3339 timestamp of the event. | 
**tool_id** | Option<**String**> |  | [optional]
**name** | Option<**String**> |  | [optional]
**clean_name** | Option<**String**> |  | [optional]
**status** | Option<[**models::ToolEventStatus**](ToolEventStatus.md)> |  | [optional]
**runtime_ms** | Option<**i64**> |  | [optional]
**payload** | Option<[**std::collections::HashMap<String, serde_json::Value>**](serde_json::Value.md)> |  | [optional]
**error** | Option<**String**> |  | [optional]
**call_id** | Option<**i32**> |  | [optional]
**model** | Option<**String**> |  | [optional]
**phase** | Option<**String**> |  | [optional]

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)


