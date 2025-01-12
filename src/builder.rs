use super::{BoltLoad, BoltLoadConfiguration};

pub struct BoltLoadBuilder {
    configuration: Option<BoltLoadConfiguration>,
}

impl Default for BoltLoadBuilder {
    fn default() -> Self {
        Self {
            configuration: None,
        }
    }
}

impl BoltLoadBuilder {
    pub fn build(self) -> BoltLoad {
        BoltLoad {
            tasks: vec![],
            configuration: self.configuration.unwrap_or_default(),
        }
    }
}
