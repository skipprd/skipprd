# ThreadStateItem

## Properties

Name | Type | Description | Notes
------------ | ------------- | ------------- | -------------
**item_id** | **String** |  | 
**kind** | **String** |  | 
**status** | **String** |  | 
**started_at** | Option<**String**> |  | [optional]
**finished_at** | Option<**String**> |  | [optional]
**runtime_ms** | Option<**i64**> |  | [optional]
**last_error** | Option<[**models::ThreadStateItemError**](ThreadStateItemError.md)> |  | [optional]
**outputs** | Option<[**std::collections::HashMap<String, serde_json::Value>**](serde_json::Value.md)> | Opaque outputs object (suite/tool specific). Keep small. | [optional]

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)


