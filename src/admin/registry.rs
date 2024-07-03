use std::collections::hash_map::{Entry, HashMap};
use std::fmt;
use std::result::Result;
use std::sync::{Arc, Mutex};

use crate::types::*;
use anyhow::*;

#[derive(Clone, Debug)]
pub struct Registry {
    /// The shared state is guarded by a mutex. This is a `std::sync::Mutex` and
    /// not a Tokio mutex. This is because there are no asynchronous operations
    /// being performed while holding the mutex. Additionally, the critical
    /// sections are very small.
    map: Arc<Mutex<HashMap<String, RegistryEntry>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            map: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn register(&self, entry: RegistryEntry) {
        let mut state = self.map.lock().unwrap();
        println!("Registering {:#?}", entry);
        match state.insert(entry.name.clone(), entry) {
            Some(old) => println!("Replaced old entry {:#?}", old),
            None => (),
        };
    }

    pub fn deregister(&self, name: String) -> anyhow::Result<()> {
        let mut state = self.map.lock().unwrap();
        match state.remove(&name) {
            Some(entry) => {
                println!("Deregistering {:#?}", entry);
                Ok(())
            }
            None => bail!("Can't deregister entry {}, it not registered", name.clone()),
        }
    }

    pub fn by_name(&self, name: String) -> anyhow::Result<RegistryEntry> {
        let mut state = self.map.lock().unwrap();
        match state.entry(name.clone()) {
            Entry::Occupied(v) => Ok(v.get().clone()),
            Entry::Vacant(_) => bail!(format!("Service {name} not registered")),
        }
    }

    pub fn by_name_many(&self, name: String) -> anyhow::Result<Vec<RegistryEntry>> {
        let state = self.map.lock().unwrap();
        let list: Vec<RegistryEntry> = state
            .values()
            .filter(|x| x.name.contains(name.as_str()))
            .map(|x| x.clone())
            .collect();
        if list.len() == 0 {
            bail!("No entries match string {}", name)
        } else {
            Ok(list)
        }
    }

    pub fn by_type_many(&self, r#type: UnitType) -> Vec<RegistryEntry> {
        let state = self.map.lock().unwrap();
        state
            .values()
            .filter(|x| x.r#type == r#type)
            .map(|x| x.clone())
            .collect()
    }

    pub fn by_type(&self, r#type: UnitType) -> anyhow::Result<RegistryEntry> {
        let vec = self.by_type_many(r#type);
        match vec.len() {
            1 => Ok(vec[0].clone()),
            0 => bail!("No service registered for"),
            _ => bail!("More than one unique services registered"), // FIXME: Fail registration, this situation should never happens
        }
    }

    pub fn contains(&self, name: String) -> bool {
        let state = self.map.lock().unwrap();
        state.contains_key(&name)
    }

    pub fn create_unique_entry_name(&self, name: String) -> String {
        let state = self.map.lock().unwrap();
        let mut counter = 0;
        loop {
            let new_name = format!("{name}@{counter}.service");
            if !state.contains_key(&new_name) {
                return new_name;
            }
            counter += 1;
        }
    }

    pub fn watch_list(&self) -> Vec<RegistryEntry> {
        let state = self.map.lock().unwrap();
        state
            .values()
            .filter(|x| x.watch)
            .map(|x| x.clone())
            .collect()
    }

    pub fn update_state(&self, name: String, status: UnitStatus) -> anyhow::Result<()> {
        let mut state = self.map.lock().unwrap();
        if let Some(e) = state.get_mut(&name) {
            e.status = status
        } else {
            bail!("Can't update state for {}, is not registered", name)
        };
        Ok(())
    }
}
