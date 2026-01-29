# UserRequest

## Properties

Name | Type | Description | Notes
------------ | ------------- | ------------- | -------------
**v** | **i32** |  | 
**cid** | **String** |  | 
**r#type** | **String** |  | 
**thread_id** | **String** |  | 
**text** | **String** | User-provided content in response to an `await_user` prompt | 
**trace** | Option<**bool**> | If true, stream `TraceResponse` frames while the agent runs. Each trace frame includes `text` plus a coarse `status` (pending|running|ok|failed). If omitted, the server inherits the last-known trace setting for the thread/connection. | [optional]

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)


