use crate::control_flow::Phase;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthoringKind {
    Cleanse,
    Model,
}

pub(crate) trait AuthoringPlanAdapter {
    fn kind(&self) -> AuthoringKind;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CleanseAdapter;

impl AuthoringPlanAdapter for CleanseAdapter {
    fn kind(&self) -> AuthoringKind {
        AuthoringKind::Cleanse
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ModelAdapter;

impl AuthoringPlanAdapter for ModelAdapter {
    fn kind(&self) -> AuthoringKind {
        AuthoringKind::Model
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum AuthoringAdapter {
    Cleanse(CleanseAdapter),
    Model(ModelAdapter),
}

impl AuthoringAdapter {
    pub(crate) fn kind(self) -> AuthoringKind {
        match self {
            Self::Cleanse(v) => v.kind(),
            Self::Model(v) => v.kind(),
        }
    }
}

pub(crate) fn adapter_for_phase(phase: Phase) -> Option<AuthoringAdapter> {
    match phase {
        Phase::CleanseAuthor => Some(AuthoringAdapter::Cleanse(CleanseAdapter)),
        Phase::ModelAuthor => Some(AuthoringAdapter::Model(ModelAdapter)),
        _ => None,
    }
}
