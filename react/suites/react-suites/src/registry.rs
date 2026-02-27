pub use react_core::suite::SuiteRegistry;

pub fn default_registry() -> SuiteRegistry {
    let mut reg = SuiteRegistry::new();
    reg.register(crate::data_engineer::DataEngineerSuite);
    reg.register(crate::kb::KbSuite);
    reg
}
