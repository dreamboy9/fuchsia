// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This mod defines the building blocks for receiving inbound communication from external
//! interfaces, such as FIDL. It also includes common implementations for working with
//! [Jobs](crate::job::Job) for incoming requests.

/// The [fidl] mod enables defining components that provide inbound communication over FIDL.
pub mod fidl;

pub mod request;
pub mod watch;

/// [Scoped] is a simple wrapper that can be used to overcome issues with using types outside this
/// crate in traits that are defined outside as well. For example, the [From] trait requires that
/// genericized type be defined within the crate. The extract method can be used to retrieve the
/// contained data.
pub struct Scoped<T>(pub T);

impl<T> Scoped<T> {
    pub fn extract(self) -> T {
        self.0
    }
}

pub mod registration {
    use super::fidl;
    use crate::base::Dependency;
    use crate::job::source::Seeder;
    use crate::service::message::Delegate;
    use fuchsia_component::server::{ServiceFsDir, ServiceObj};
    use std::collections::HashSet;

    /// [Registrar] defines the medium over which communication occurs. Each entry includes a
    /// closure that takes a specific set of inputs necessary to bring up that particular
    /// communication.
    pub enum Registrar {
        Fidl(fidl::Register),
        /// This value is reserved for testing purposes.
        #[cfg(test)]
        Test(Box<dyn FnOnce() + Send + Sync>),
        #[cfg(test)]
        TestWithDelegate(Box<dyn FnOnce(&Delegate) + Send + Sync>),
        #[cfg(test)]
        TestWithSeeder(Box<dyn FnOnce(&Seeder) + Send + Sync>),
    }

    impl Registrar {
        /// Brings up the communication by supplying the subset of needed inputs to the particular
        /// [Registrar].
        pub fn register<'a>(
            self,
            delegate: &Delegate,
            job_seeder: &Seeder,
            service_dir: &mut ServiceFsDir<'_, ServiceObj<'a, ()>>,
        ) {
            match self {
                Registrar::Fidl(register_fn) => {
                    register_fn(delegate, job_seeder, service_dir);
                }
                #[cfg(test)]
                Registrar::Test(register_fn) => {
                    register_fn();
                }
                #[cfg(test)]
                Registrar::TestWithDelegate(register_fn) => {
                    register_fn(delegate);
                }
                #[cfg(test)]
                Registrar::TestWithSeeder(register_fn) => {
                    register_fn(job_seeder);
                }
            }
        }
    }

    /// [Registrant] brings up an inbound interface in the service.
    pub struct Registrant {
        /// A list of [Dependencies](Dependency) the registrant relies on being present in order to
        /// function.
        dependencies: HashSet<Dependency>,
        /// The [Registrar] responsible for bringing up the interface.
        registrar: Registrar,
    }

    impl Registrant {
        pub fn get_dependencies(&self) -> &HashSet<Dependency> {
            &self.dependencies
        }

        pub fn register<'a>(
            self,
            delegate: &Delegate,
            job_seeder: &Seeder,
            service_dir: &mut ServiceFsDir<'_, ServiceObj<'a, ()>>,
        ) {
            self.registrar.register(delegate, job_seeder, service_dir);
        }
    }

    pub struct Builder {
        registrar: Registrar,
        dependencies: HashSet<Dependency>,
    }

    impl Builder {
        pub fn new(registrar: Registrar) -> Self {
            Self { registrar, dependencies: HashSet::new() }
        }

        pub fn add_dependency(mut self, dependency: Dependency) -> Self {
            self.dependencies.insert(dependency);

            self
        }

        pub fn build(self) -> Registrant {
            Registrant { registrar: self.registrar, dependencies: self.dependencies }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::registration::{Builder, Registrar};
    use crate::base::{Dependency, Entity, SettingType};
    use crate::job::source::Seeder;
    use crate::message::base::MessengerType;
    use crate::service;
    use fuchsia_component::server::ServiceFs;
    use matches::assert_matches;

    #[fuchsia_async::run_until_stalled(test)]
    async fn test_registration() {
        let (tx, rx) = futures::channel::oneshot::channel::<()>();
        let dependency = Dependency::Entity(Entity::Handler(SettingType::Unknown));
        let registrant = Builder::new(Registrar::Test(Box::new(move || {
            assert!(tx.send(()).is_ok());
        })))
        .add_dependency(dependency)
        .build();

        let mut fs = ServiceFs::new();

        // Verify added dependency.
        assert!(registrant.get_dependencies().contains(&dependency));

        let delegate = service::message::create_hub();
        let job_manager_signature = delegate
            .create(MessengerType::Unbound)
            .await
            .expect("messenger should be created")
            .0
            .get_signature();
        let job_seeder = Seeder::new(&delegate, job_manager_signature).await;

        // Register and consume Registrant.
        registrant.register(&service::message::create_hub(), &job_seeder, &mut fs.root_dir());

        // Verify registration occurred.
        assert_matches!(rx.await, Ok(()));
    }
}
