# SuiteProgressResponse

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
**suite_id** | **String** | Suite id currently running (e.g. data_engineer) | 
**phases** | **Vec<String>** | Ordered list of phases for the current suite (best-effort) | 
**completed** | **Vec<String>** | Phases considered complete (best-effort) | 
**current** | **String** | Current phase name | 

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)


