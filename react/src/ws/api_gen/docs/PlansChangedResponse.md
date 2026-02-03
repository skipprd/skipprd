# PlansChangedResponse

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
**changed** | **Vec<String>** | Which plan kinds changed. UI should fetch `plans` to get latest snapshots. | 
**cleanse_plan_key** | Option<**String**> |  | [optional]
**model_plan_key** | Option<**String**> |  | [optional]

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)


