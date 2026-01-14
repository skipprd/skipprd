#[allow(dead_code)]
pub struct SequencerHandle;

#[allow(dead_code)]
pub async fn ensure_sequencer() -> Result<SequencerHandle, String> {
    Ok(SequencerHandle)
}

#[allow(dead_code)]
pub async fn apply_evolutions() -> Result<(), String> {
    Ok(())
}

#[allow(dead_code)]
pub async fn propose_and_wait(_sql: &str) -> Result<(), String> {
    Ok(())
}


