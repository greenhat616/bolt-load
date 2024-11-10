use crate::client::BoltLoaderMode;

pub mod multi_thread;
pub mod single_thread;

pub struct BoltLoaderTaskManager {
    adaptor: Box<dyn super::adaptor::BoltLoadAdaptor + Sync + 'static>,
}

impl BoltLoaderTaskManager {
    pub fn new_single<T>(adaptor: T, save_path: &String, url: &String) -> Self
    where
        T: super::adaptor::BoltLoadAdaptor + Sync + 'static,
    {
        todo!();
    }

    pub fn new_multi<T>(adaptor: T, save_path: &String, url: &String) -> Vec<Self>
    where
        T: super::adaptor::BoltLoadAdaptor + Sync + 'static,
    {
        todo!();
    }
}
