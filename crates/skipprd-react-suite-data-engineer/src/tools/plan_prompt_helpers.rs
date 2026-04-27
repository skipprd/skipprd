pub(crate) fn render_plan_driven_instructions(
    invariants: &[String],
    checklist: &[crate::plan::PlanChecklistItem],
) -> String {
    let mut out = String::new();
    if !invariants.is_empty() {
        out.push_str("Plan invariants (MUST satisfy):\n");
        for inv in invariants.iter() {
            let t = inv.trim();
            if t.is_empty() {
                continue;
            }
            out.push_str("- ");
            out.push_str(t);
            out.push('\n');
        }
        out.push('\n');
    }
    let mut any = false;
    for it in checklist.iter() {
        let has_details = it
            .details
            .as_ref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        let include = it.status != crate::plan::ChecklistItemStatus::Done || has_details;
        if !include {
            continue;
        }
        if !any {
            out.push_str("Plan checklist (remaining work):\n");
            any = true;
        }
        let origin = match it.origin {
            crate::plan::ChecklistOrigin::Initial => "initial",
        };
        out.push_str("- ");
        out.push_str(it.label.trim());
        out.push_str(" (id=");
        out.push_str(it.checklist_item_id.trim());
        out.push_str(", status=");
        out.push_str(&format!("{:?}", it.status));
        out.push_str(", origin=");
        out.push_str(origin);
        out.push(')');
        if let Some(d) = it
            .details
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            out.push_str(": ");
            out.push_str(d);
        }
        out.push('\n');
    }
    if any {
        out.push('\n');
    }
    out.trim().to_string()
}

pub(crate) fn combine_instructions(user_instructions: &str, plan_instructions: &str) -> String {
    let ui = user_instructions.trim();
    let pi = plan_instructions.trim();
    if ui.is_empty() && pi.is_empty() {
        return String::new();
    }
    if ui.is_empty() {
        return pi.to_string();
    }
    if pi.is_empty() {
        return ui.to_string();
    }
    format!("User instructions:\n{}\n\n{}", ui, pi)
}
