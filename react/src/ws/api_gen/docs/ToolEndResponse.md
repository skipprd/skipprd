# ToolEndResponse

## Properties

Name | Type | Description | Notes
------------ | ------------- | ------------- | -------------
**v** | **i32** |  | 
**r#type** | **String** |  | 
**server_time** | **String** |  | 
**seq** | **i32** |  | 
**thread_id** | **String** |  | 
**thread_seq** | Option<**i32**> |  | [optional]
**for_cid** | Option<**String**> |  | [optional]
**tool_id** | **String** |  | 
**name** | **String** |  | 
**clean_name** | Option<**String**> | Human-readable, short, self-documenting label for UI (e.g. \"Read dbt_project.yml\"). | [optional]
**phase** | Option<**String**> |  | [optional]
**status** | [**models::ToolEventStatus**](ToolEventStatus.md) |  | 
**payload** | Option<[**std::collections::HashMap<String, serde_json::Value>**](serde_json::Value.md)> |  | [optional]
**error** | Option<**String**> |  | [optional]

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)


