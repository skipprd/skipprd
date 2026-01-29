# NewRequest

## Properties

Name | Type | Description | Notes
------------ | ------------- | ------------- | -------------
**v** | **i32** |  | 
**cid** | **String** |  | 
**r#type** | **String** |  | 
**question** | **String** | User's question to start a new thread | 
**suite_id** | **String** | Suite id to use (e.g. data_engineer | kb) | 
**agent_type** | **String** | Agent/mode to use (ask | cleanse | model | kb | agent | review). Required. | 
**trace** | Option<**bool**> | If true, stream `TraceResponse` frames while the agent runs. Each trace frame includes `text` plus a coarse `status` (pending|running|ok|failed). | [optional]

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)


