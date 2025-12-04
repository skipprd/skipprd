pub async fn run(_pipeline: &str, prompt: &str) -> Result<(), String> {
    use uuid::Uuid;
    let thread_id = Uuid::new_v4().to_string();
    let frames = crate::flows::model::run(&thread_id, prompt).await?;
    for f in frames {
        if let crate::flows::adapter::FlowFrame::Final { answer, .. } = f {
            println!("{}", answer);
            break;
        }
    }
    Ok(())
}


