# PhaseResponse

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
**step_idx** | **i32** | Index of the backing ThreadStep::Phase in the thread log. | 
**phase** | **String** |  | 
**from_phase** | Option<**String**> |  | [optional]
**reason_code** | Option<[**models::PhaseReasonCode**](PhaseReasonCode.md)> |  | [optional]
**reason_detail** | Option<[**models::PhaseReasonDetail**](PhaseReasonDetail.md)> |  | [optional]
**ts** | **String** | RFC3339 timestamp stored in the thread log for this phase transition. | 
**runs** | [**Vec<models::PhaseRun>**](PhaseRun.md) | List of start/end windows for this phase across the thread lifetime. Each re-entry into the phase appends a new run.  | 
**total_runtime_ms** | **i64** | Sum of all completed run durations for this phase (ms). | 
**from_phase_runs** | Option<[**Vec<models::PhaseRun>**](PhaseRun.md)> | If `from_phase` is present, this is the updated run list for the phase being exited.  | [optional]
**from_phase_total_runtime_ms** | Option<**i64**> | If `from_phase` is present, sum of all completed run durations for it (ms). | [optional]

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)


