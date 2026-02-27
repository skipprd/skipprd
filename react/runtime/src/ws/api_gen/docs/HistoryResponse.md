# HistoryResponse

## Properties

Name | Type | Description | Notes
------------ | ------------- | ------------- | -------------
**v** | **i32** |  | 
**r#type** | **String** |  | 
**server_time** | **String** |  | 
**seq** | **i32** |  | 
**thread_id** | **String** |  | 
**title** | Option<**String**> |  | [optional]
**suite_id** | Option<**String**> | Current suite for the thread (derived from thread history) | [optional]
**agent_type** | Option<**String**> | Current agent/mode for the thread (derived from thread history) | [optional]
**messages** | [**Vec<models::HistoryResponseMessagesInner>**](HistoryResponse_messages_inner.md) |  | 
**next_before_thread_seq** | Option<**i32**> |  | [optional]

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)


