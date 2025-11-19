pub enum FlowFrame {
	Final { answer: String, sql: Option<String> },
	AwaitUser { prompt: String },
	AwaitApproval { prompt: String },
	Processing { note: Option<String> },
}

#[allow(unused_variables)]
pub trait FlowEmitter {
	fn emit(&mut self, frame: &FlowFrame) {}
}


