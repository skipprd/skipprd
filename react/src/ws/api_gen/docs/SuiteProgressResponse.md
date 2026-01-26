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
**phases** | **Vec<String>** | Ordered list of phases for the current suite (best-effort).  For `data_engineer`, phases include plan phases: - preflight - cleanse_plan - cleanse_author - cleanse_validate - cleanse_review - model_plan - model_author - model_validate - model_review - publish_await_approval - publish - post_publish_review - done  | 
**completed** | **Vec<String>** | Phases considered complete (best-effort) | 
**current** | **String** | Current phase name (for `data_engineer`, see `phases` list for known values including plan phases) | 

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)


