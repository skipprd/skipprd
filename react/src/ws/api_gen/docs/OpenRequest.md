# OpenRequest

## Properties

Name | Type | Description | Notes
------------ | ------------- | ------------- | -------------
**v** | **i32** |  | 
**cid** | **String** |  | 
**r#type** | **String** |  | 
**thread_id** | **String** | Existing thread id returned by a previous interaction | 
**question** | Option<**String**> | Optional nudge to resume the thread (default is \"Continue.\") | [optional]
**suite_id** | **String** | Suite id to use (e.g. data_engineer | kb) | 
**agent_type** | **String** | Agent to use for this message; if differs from current, a switch_agent step is recorded. | 
**trace** | Option<**bool**> | If true, stream `TraceResponse` frames while the agent runs. Each trace frame includes `text` plus a coarse `status` (pending|running|ok|failed). If omitted, the server inherits the last-known trace setting for the thread/connection. | [optional]

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)


