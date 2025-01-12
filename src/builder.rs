use super::{BoltLoad, BoltLoadConfiguration};

#[derive(Default)]
pub struct BoltLoadBuilder {
    configuration: Option<BoltLoadConfiguration>,
}


impl BoltLoadBuilder {
    pub fn build(self) -> BoltLoad {
        BoltLoad {
            tasks: vec![],
            configuration: self.configuration.unwrap_or_default(),
        }
    }
}
