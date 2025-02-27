// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// This declaration is required to support the `select!`.
#![recursion_limit = "256"]

use {
    crate::accessibility::accessibility_controller::AccessibilityController,
    crate::account::account_controller::AccountController,
    crate::agent::authority::Authority,
    crate::agent::{BlueprintHandle as AgentBlueprintHandle, Lifespan},
    crate::audio::audio_controller::AudioController,
    crate::audio::policy::audio_policy_handler::AudioPolicyHandler,
    crate::base::{Dependency, Entity, SettingType},
    crate::config::base::{AgentType, ControllerFlag},
    crate::device::device_controller::DeviceController,
    crate::display::display_controller::{DisplayController, ExternalBrightnessControl},
    crate::display::light_sensor_controller::LightSensorController,
    crate::do_not_disturb::do_not_disturb_controller::DoNotDisturbController,
    crate::factory_reset::factory_reset_controller::FactoryResetController,
    crate::handler::base::GenerateHandler,
    crate::handler::device_storage::DeviceStorageFactory,
    crate::handler::setting_handler::persist::Handler as DataHandler,
    crate::handler::setting_handler_factory_impl::SettingHandlerFactoryImpl,
    crate::handler::setting_proxy::SettingProxy,
    crate::ingress::fidl,
    crate::ingress::registration::Registrant,
    crate::input::input_controller::InputController,
    crate::intl::intl_controller::IntlController,
    crate::job::manager::Manager,
    crate::job::source::Seeder,
    crate::light::light_controller::LightController,
    crate::monitor::base as monitor_base,
    crate::night_mode::night_mode_controller::NightModeController,
    crate::policy::policy_handler,
    crate::policy::policy_handler_factory_impl::PolicyHandlerFactoryImpl,
    crate::policy::policy_proxy::PolicyProxy,
    crate::policy::PolicyType,
    crate::power::power_controller::PowerController,
    crate::privacy::privacy_controller::PrivacyController,
    crate::service::message::Delegate,
    crate::service_context::GenerateService,
    crate::service_context::ServiceContext,
    crate::setup::setup_controller::SetupController,
    anyhow::{format_err, Context, Error},
    fidl_fuchsia_settings_policy::VolumePolicyControllerRequestStream,
    fuchsia_async as fasync,
    fuchsia_component::server::{NestedEnvironment, ServiceFs, ServiceFsDir, ServiceObj},
    fuchsia_inspect::component,
    fuchsia_zircon::{Duration, DurationNum},
    futures::lock::Mutex,
    futures::StreamExt,
    handler::setting_handler::Handler,
    serde::Deserialize,
    std::collections::{HashMap, HashSet},
    std::sync::atomic::AtomicU64,
    std::sync::Arc,
};

mod accessibility;
mod account;
mod audio;
mod clock;
mod device;
mod display;
mod do_not_disturb;
mod event;
mod factory_reset;
mod fidl_processor;
mod hanging_get_handler;
mod input;
mod intl;
mod job;
mod light;
mod night_mode;
mod policy;
mod power;
mod privacy;
mod service;
mod setup;
pub mod task;

pub use display::display_configuration::DisplayConfiguration;
pub use display::LightSensorConfig;
pub use input::input_device_configuration::InputConfiguration;
pub use light::light_hardware_configuration::LightHardwareConfiguration;
pub use service::{Address, Payload, Role};

pub mod agent;
pub mod base;
pub mod config;
pub mod fidl_common;
pub mod handler;
pub mod ingress;
pub mod message;
pub mod monitor;
pub mod service_context;
pub mod storage;

/// This value represents the duration the proxy will wait after the last request
/// before initiating the teardown of the controller. If a request is received
/// before the timeout triggers, then the timeout will be canceled.
// The value of 5 seconds was chosen arbitrarily to allow some time between manual
// button presses that occurs for some settings.
pub(crate) const DEFAULT_TEARDOWN_TIMEOUT: Duration = Duration::from_seconds(5);
const DEFAULT_SETTING_PROXY_MAX_ATTEMPTS: u64 = 3;
const DEFAULT_SETTING_PROXY_RESPONSE_TIMEOUT_MS: i64 = 10_000;

/// A common trigger for exiting.
pub type ExitSender = futures::channel::mpsc::UnboundedSender<()>;

/// Runtime defines where the environment will exist. Service is meant for
/// production environments and will hydrate components to be discoverable as
/// an environment service. Nested creates a service only usable in the scope
/// of a test.
#[derive(PartialEq)]
enum Runtime {
    Service,
    Nested(&'static str),
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct AgentConfiguration {
    pub agent_types: HashSet<AgentType>,
}

#[derive(PartialEq, Debug, Clone, Deserialize)]
pub struct EnabledServicesConfiguration {
    pub services: HashSet<SettingType>,
}

impl EnabledServicesConfiguration {
    pub fn with_services(services: HashSet<SettingType>) -> Self {
        Self { services }
    }
}

#[derive(PartialEq, Debug, Clone, Deserialize)]
pub struct EnabledPoliciesConfiguration {
    pub policies: HashSet<PolicyType>,
}

impl EnabledPoliciesConfiguration {
    pub fn with_policies(policies: HashSet<PolicyType>) -> Self {
        Self { policies }
    }
}

#[derive(Default, Debug, Clone, Deserialize)]
pub struct ServiceFlags {
    pub controller_flags: HashSet<ControllerFlag>,
}

#[derive(PartialEq, Debug, Default, Clone)]
pub struct ServiceConfiguration {
    agent_types: HashSet<AgentType>,
    fidl_interfaces: HashSet<fidl::Interface>,
    policies: HashSet<PolicyType>,
    controller_flags: HashSet<ControllerFlag>,
}

impl ServiceConfiguration {
    pub fn from(
        agent_types: AgentConfiguration,
        services: EnabledServicesConfiguration,
        policies: EnabledPoliciesConfiguration,
        flags: ServiceFlags,
    ) -> Self {
        let fidl_interfaces: HashSet<fidl::Interface> =
            services.services.iter().map(|x| x.into()).collect();

        let mut return_val = Self {
            agent_types: agent_types.agent_types,
            fidl_interfaces: HashSet::new(),
            policies: policies.policies,
            controller_flags: flags.controller_flags,
        };

        return_val.set_fidl_interfaces(fidl_interfaces);

        return_val
    }

    fn set_fidl_interfaces(&mut self, interfaces: HashSet<fidl::Interface>) {
        self.fidl_interfaces = interfaces;

        let display_subinterfaces = [
            fidl::Interface::Display(fidl::display::InterfaceFlags::LIGHT_SENSOR),
            fidl::Interface::Display(fidl::display::InterfaceFlags::BASE),
        ]
        .iter()
        .cloned()
        .collect();

        // Consolidate display type.
        // TODO(fxbug.dev/76991): Remove this special handling once light sensor is its own FIDL
        // interface.
        if self.fidl_interfaces.is_superset(&display_subinterfaces) {
            self.fidl_interfaces = &self.fidl_interfaces - &display_subinterfaces;
            self.fidl_interfaces.insert(fidl::Interface::Display(
                fidl::display::InterfaceFlags::BASE | fidl::display::InterfaceFlags::LIGHT_SENSOR,
            ));
        }
    }

    fn set_policies(&mut self, policies: HashSet<PolicyType>) {
        self.policies = policies;
    }

    fn set_controller_flags(&mut self, controller_flags: HashSet<ControllerFlag>) {
        self.controller_flags = controller_flags;
    }
}

/// Environment is handed back when an environment is spawned from the
/// EnvironmentBuilder. A nested environment (if available) is returned,
/// along with a receiver to be notified when initialization/setup is
/// complete.
pub struct Environment {
    pub nested_environment: Option<NestedEnvironment>,
    pub delegate: Delegate,
    pub entities: HashSet<Entity>,
    pub job_seeder: Seeder,
}

impl Environment {
    pub fn new(
        nested_environment: Option<NestedEnvironment>,
        delegate: Delegate,
        job_seeder: Seeder,
        entities: HashSet<Entity>,
    ) -> Environment {
        Environment { nested_environment, delegate, job_seeder, entities }
    }
}

macro_rules! register_handler {
    (
        $components:ident,
        $storage_factory:ident,
        $handler_factory:ident,
        $setting_type:expr,
        $controller:ty,
        $spawn_method:expr
    ) => {
        if $components.contains(&$setting_type) {
            $storage_factory
                .initialize::<$controller>()
                .await
                .expect("should be initializing still");
        }
        $handler_factory.register($setting_type, Box::new($spawn_method));
    };
}

/// The [EnvironmentBuilder] aggregates the parameters surrounding an [environment](Environment) and
/// ultimately spawns an environment based on them.
pub struct EnvironmentBuilder<T: DeviceStorageFactory + Send + Sync + 'static> {
    configuration: Option<ServiceConfiguration>,
    agent_blueprints: Vec<AgentBlueprintHandle>,
    agent_mapping_func: Option<Box<dyn Fn(AgentType) -> AgentBlueprintHandle>>,
    event_subscriber_blueprints: Vec<event::subscriber::BlueprintHandle>,
    storage_factory: Arc<T>,
    generate_service: Option<GenerateService>,
    registrants: Vec<Registrant>,
    settings: Vec<SettingType>,
    handlers: HashMap<SettingType, GenerateHandler>,
    resource_monitors: Vec<monitor_base::monitor::Generate>,
}

impl<T: DeviceStorageFactory + Send + Sync + 'static> EnvironmentBuilder<T> {
    pub fn new(storage_factory: Arc<T>) -> EnvironmentBuilder<T> {
        EnvironmentBuilder {
            configuration: None,
            agent_blueprints: vec![],
            agent_mapping_func: None,
            event_subscriber_blueprints: vec![],
            storage_factory,
            generate_service: None,
            handlers: HashMap::new(),
            registrants: vec![],
            settings: vec![],
            resource_monitors: vec![],
        }
    }

    pub fn handler(
        mut self,
        setting_type: SettingType,
        generate_handler: GenerateHandler,
    ) -> EnvironmentBuilder<T> {
        self.handlers.insert(setting_type, generate_handler);
        self
    }

    /// A service generator to be used as an overlay on the ServiceContext.
    pub fn service(mut self, generate_service: GenerateService) -> EnvironmentBuilder<T> {
        self.generate_service = Some(generate_service);
        self
    }

    /// A preset configuration to load preset parameters as a base.
    pub fn configuration(mut self, configuration: ServiceConfiguration) -> EnvironmentBuilder<T> {
        self.configuration = Some(configuration);
        self
    }

    pub fn fidl_interfaces(mut self, interfaces: &[fidl::Interface]) -> EnvironmentBuilder<T> {
        if self.configuration.is_none() {
            self.configuration = Some(ServiceConfiguration::default());
        }

        if let Some(c) = self.configuration.as_mut() {
            c.set_fidl_interfaces(interfaces.iter().copied().collect());
        }

        self
    }

    pub fn registrants(mut self, mut registrants: Vec<Registrant>) -> EnvironmentBuilder<T> {
        self.registrants.append(&mut registrants);

        self
    }

    /// Setting types to participate.
    pub fn settings(mut self, settings: &[SettingType]) -> EnvironmentBuilder<T> {
        self.settings.extend(settings);

        self
    }

    /// Sets policies types that are enabled.
    pub fn policies(mut self, policies: &[PolicyType]) -> EnvironmentBuilder<T> {
        if self.configuration.is_none() {
            self.configuration = Some(ServiceConfiguration::default());
        }

        if let Some(c) = self.configuration.as_mut() {
            c.set_policies(policies.iter().copied().collect());
        }

        self
    }

    /// Setting types to participate with customized controllers.
    pub fn flags(mut self, controller_flags: &[ControllerFlag]) -> EnvironmentBuilder<T> {
        if self.configuration.is_none() {
            self.configuration = Some(ServiceConfiguration::default());
        }

        if let Some(c) = self.configuration.as_mut() {
            c.set_controller_flags(controller_flags.iter().copied().collect());
        }

        self
    }

    pub fn agent_mapping<F>(mut self, agent_mapping_func: F) -> EnvironmentBuilder<T>
    where
        F: Fn(AgentType) -> AgentBlueprintHandle + 'static,
    {
        self.agent_mapping_func = Some(Box::new(agent_mapping_func));
        self
    }

    pub fn agents(mut self, blueprints: &[AgentBlueprintHandle]) -> EnvironmentBuilder<T> {
        self.agent_blueprints.append(&mut blueprints.to_vec());
        self
    }

    pub fn resource_monitors(
        mut self,
        monitors: &[monitor_base::monitor::Generate],
    ) -> EnvironmentBuilder<T> {
        self.resource_monitors.append(&mut monitors.to_vec());
        self
    }

    /// Event subscribers to participate
    pub fn event_subscribers(
        mut self,
        subscribers: &[event::subscriber::BlueprintHandle],
    ) -> EnvironmentBuilder<T> {
        self.event_subscriber_blueprints.append(&mut subscribers.to_vec());
        self
    }

    async fn prepare_env(
        mut self,
        runtime: Runtime,
    ) -> Result<(ServiceFs<ServiceObj<'static, ()>>, Delegate, Seeder, HashSet<Entity>), Error>
    {
        let mut fs = ServiceFs::new();

        let service_dir;
        if runtime == Runtime::Service {
            // Initialize inspect.
            inspect_runtime::serve(component::inspector(), &mut fs).ok();

            service_dir = fs.dir("svc");
        } else {
            service_dir = fs.root_dir();
        }

        // Define top level MessageHub for service communication.
        let delegate = service::message::create_hub();

        let (agent_types, fidl_interfaces, policies, flags) = match self.configuration {
            Some(configuration) => (
                configuration.agent_types,
                configuration.fidl_interfaces,
                configuration.policies,
                configuration.controller_flags,
            ),
            _ => (HashSet::new(), HashSet::new(), HashSet::new(), HashSet::new()),
        };

        self.registrants
            .extend(fidl_interfaces.into_iter().map(|x| x.into()).collect::<Vec<Registrant>>());

        let mut settings = HashSet::new();
        settings.extend(self.settings);

        for registrant in &self.registrants {
            for dependency in registrant.get_dependencies() {
                match dependency {
                    Dependency::Entity(Entity::Handler(setting_type)) => {
                        settings.insert(*setting_type);
                    }
                }
            }
        }

        let service_context =
            Arc::new(ServiceContext::new(self.generate_service, Some(delegate.clone())));

        let context_id_counter = Arc::new(AtomicU64::new(1));

        let mut handler_factory = SettingHandlerFactoryImpl::new(
            settings.clone(),
            Arc::clone(&service_context),
            context_id_counter.clone(),
        );

        // Create the policy handler factory and register policy handlers.
        let mut policy_handler_factory = PolicyHandlerFactoryImpl::new(
            policies.clone(),
            settings.clone(),
            self.storage_factory.clone(),
            context_id_counter,
        );
        // If policy registration becomes configurable, then this initialization needs to be made
        // configurable with the registration.
        PolicyType::Audio
            .initialize_storage(&self.storage_factory)
            .await
            .expect("was not able to initialize storage for audio policy");
        policy_handler_factory.register(
            PolicyType::Audio,
            Box::new(policy_handler::create_handler::<AudioPolicyHandler, _>),
        );

        EnvironmentBuilder::get_configuration_handlers(
            &settings,
            Arc::clone(&self.storage_factory),
            &flags,
            &mut handler_factory,
        )
        .await;

        // Override the configuration handlers with any custom handlers specified
        // in the environment.
        for (setting_type, handler) in self.handlers {
            handler_factory.register(setting_type, handler);
        }

        for agent_type in &agent_types {
            agent_type
                .initialize_storage(&self.storage_factory)
                .await
                .expect("unable to initialize storage for agent");
        }

        let agent_blueprints = self
            .agent_mapping_func
            .map(|agent_mapping_func| {
                agent_types.into_iter().map(|agent_type| (agent_mapping_func)(agent_type)).collect()
            })
            .unwrap_or(self.agent_blueprints);

        let job_manager_signature = Manager::spawn(&delegate).await;
        let job_seeder = Seeder::new(&delegate, job_manager_signature).await;

        let entities = create_environment(
            service_dir,
            delegate.clone(),
            job_seeder.clone(),
            settings,
            self.registrants,
            policies,
            agent_blueprints,
            self.resource_monitors,
            self.event_subscriber_blueprints,
            service_context,
            Arc::new(Mutex::new(handler_factory)),
            Arc::new(Mutex::new(policy_handler_factory)),
            self.storage_factory,
        )
        .await
        .map_err(|err| format_err!("could not create environment: {:?}", err))?;

        Ok((fs, delegate, job_seeder, entities))
    }

    pub fn spawn(self, mut executor: fasync::LocalExecutor) -> Result<(), Error> {
        match executor.run_singlethreaded(self.prepare_env(Runtime::Service)) {
            Ok((mut fs, ..)) => {
                fs.take_and_serve_directory_handle().expect("could not service directory handle");
                let () = executor.run_singlethreaded(fs.collect());

                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub async fn spawn_nested(self, env_name: &'static str) -> Result<Environment, Error> {
        match self.prepare_env(Runtime::Nested(env_name)).await {
            Ok((mut fs, delegate, job_seeder, entities)) => {
                let nested_environment = Some(fs.create_salted_nested_environment(&env_name)?);
                fasync::Task::spawn(fs.collect()).detach();

                Ok(Environment::new(nested_environment, delegate, job_seeder, entities))
            }
            Err(error) => Err(error),
        }
    }

    /// Spawns a nested environment and returns the associated
    /// NestedEnvironment. Note that this is a helper function that provides a
    /// shortcut for calling EnvironmentBuilder::name() and
    /// EnvironmentBuilder::spawn().
    pub async fn spawn_and_get_nested_environment(
        self,
        env_name: &'static str,
    ) -> Result<NestedEnvironment, Error> {
        let environment = self.spawn_nested(env_name).await?;

        environment.nested_environment.ok_or_else(|| format_err!("nested environment not created"))
    }

    async fn get_configuration_handlers(
        components: &HashSet<SettingType>,
        storage_factory: Arc<T>,
        controller_flags: &HashSet<ControllerFlag>,
        factory_handle: &mut SettingHandlerFactoryImpl,
    ) {
        // Power
        register_handler!(
            components,
            storage_factory,
            factory_handle,
            SettingType::Power,
            PowerController,
            Handler::<PowerController>::spawn
        );
        // Accessibility
        register_handler!(
            components,
            storage_factory,
            factory_handle,
            SettingType::Accessibility,
            AccessibilityController,
            DataHandler::<AccessibilityController>::spawn
        );
        // Account
        register_handler!(
            components,
            storage_factory,
            factory_handle,
            SettingType::Account,
            AccountController,
            Handler::<AccountController>::spawn
        );
        // Audio
        register_handler!(
            components,
            storage_factory,
            factory_handle,
            SettingType::Audio,
            AudioController,
            DataHandler::<AudioController>::spawn
        );
        // Device
        register_handler!(
            components,
            storage_factory,
            factory_handle,
            SettingType::Device,
            DeviceController,
            Handler::<DeviceController>::spawn
        );
        // Display
        register_handler!(
            components,
            storage_factory,
            factory_handle,
            SettingType::Display,
            DisplayController,
            if controller_flags.contains(&ControllerFlag::ExternalBrightnessControl) {
                DataHandler::<DisplayController<ExternalBrightnessControl>>::spawn
            } else {
                DataHandler::<DisplayController>::spawn
            }
        );
        // Light
        register_handler!(
            components,
            storage_factory,
            factory_handle,
            SettingType::Light,
            LightController,
            DataHandler::<LightController>::spawn
        );
        // Light sensor
        register_handler!(
            components,
            storage_factory,
            factory_handle,
            SettingType::LightSensor,
            LightSensorController,
            Handler::<LightSensorController>::spawn
        );
        // Input
        register_handler!(
            components,
            storage_factory,
            factory_handle,
            SettingType::Input,
            InputController,
            DataHandler::<InputController>::spawn
        );
        // Intl
        register_handler!(
            components,
            storage_factory,
            factory_handle,
            SettingType::Intl,
            IntlController,
            DataHandler::<IntlController>::spawn
        );
        // Do not disturb
        register_handler!(
            components,
            storage_factory,
            factory_handle,
            SettingType::DoNotDisturb,
            DoNotDisturbController,
            DataHandler::<DoNotDisturbController>::spawn
        );
        // Factory Reset
        register_handler!(
            components,
            storage_factory,
            factory_handle,
            SettingType::FactoryReset,
            FactoryResetController,
            DataHandler::<FactoryResetController>::spawn
        );
        // Night mode
        register_handler!(
            components,
            storage_factory,
            factory_handle,
            SettingType::NightMode,
            NightModeController,
            DataHandler::<NightModeController>::spawn
        );
        // Privacy
        register_handler!(
            components,
            storage_factory,
            factory_handle,
            SettingType::Privacy,
            PrivacyController,
            DataHandler::<PrivacyController>::spawn
        );
        // Setup
        register_handler!(
            components,
            storage_factory,
            factory_handle,
            SettingType::Setup,
            SetupController,
            DataHandler::<SetupController>::spawn
        );
    }
}

/// Brings up the settings service environment.
///
/// This method generates the necessary infrastructure to support the settings
/// service (handlers, agents, etc.) and brings up the components necessary to
/// support the components specified in the components HashSet.
#[allow(clippy::too_many_arguments)]
async fn create_environment<'a, T: DeviceStorageFactory + Send + Sync + 'static>(
    mut service_dir: ServiceFsDir<'_, ServiceObj<'a, ()>>,
    delegate: service::message::Delegate,
    job_seeder: Seeder,
    components: HashSet<SettingType>,
    registrants: Vec<Registrant>,
    policies: HashSet<PolicyType>,
    agent_blueprints: Vec<AgentBlueprintHandle>,
    resource_monitor_generators: Vec<monitor_base::monitor::Generate>,
    event_subscriber_blueprints: Vec<event::subscriber::BlueprintHandle>,
    service_context: Arc<ServiceContext>,
    handler_factory: Arc<Mutex<SettingHandlerFactoryImpl>>,
    policy_handler_factory: Arc<Mutex<PolicyHandlerFactoryImpl<T>>>,
    storage_factory: Arc<T>,
) -> Result<HashSet<Entity>, Error> {
    for blueprint in event_subscriber_blueprints {
        blueprint.create(delegate.clone()).await;
    }

    let monitor_actor = if resource_monitor_generators.is_empty() {
        None
    } else {
        Some(monitor::environment::Builder::new().add_monitors(resource_monitor_generators).build())
    };

    let mut entities = HashSet::new();

    // TODO(fxbug.dev/58893): make max attempts a configurable option.
    // TODO(fxbug.dev/59174): make setting proxy response timeout and retry configurable.
    for setting_type in &components {
        SettingProxy::create(
            *setting_type,
            handler_factory.clone(),
            delegate.clone(),
            DEFAULT_SETTING_PROXY_MAX_ATTEMPTS,
            DEFAULT_TEARDOWN_TIMEOUT,
            Some(DEFAULT_SETTING_PROXY_RESPONSE_TIMEOUT_MS.millis()),
            true,
        )
        .await?;

        entities.insert(Entity::Handler(*setting_type));
    }

    for policy_type in &policies {
        let setting_type = policy_type.setting_type();
        if components.contains(&setting_type) {
            PolicyProxy::create(*policy_type, policy_handler_factory.clone(), delegate.clone())
                .await?;
        }
    }

    let mut agent_authority =
        Authority::create(delegate.clone(), components.clone(), policies, monitor_actor).await?;

    for registrant in registrants {
        if registrant.get_dependencies().iter().all(|dependency| dependency.is_fulfilled(&entities))
        {
            registrant.register(&delegate, &job_seeder, &mut service_dir);
        }
    }

    // TODO(fxbug.dev/60925): allow configuration of policy API
    service_dir.add_fidl_service(move |stream: VolumePolicyControllerRequestStream| {
        crate::audio::policy::volume_policy_fidl_handler::fidl_io::spawn(delegate.clone(), stream);
    });

    // The service does not work without storage, so ensure it is always included first.
    agent_authority
        .register(Arc::new(crate::agent::storage_agent::Blueprint::new(storage_factory)))
        .await;

    for blueprint in agent_blueprints {
        agent_authority.register(blueprint).await;
    }

    // Execute initialization agents sequentially
    agent_authority
        .execute_lifespan(Lifespan::Initialization, Arc::clone(&service_context), true)
        .await
        .context("Agent initialization failed")?;

    // Execute service agents concurrently
    agent_authority
        .execute_lifespan(Lifespan::Service, Arc::clone(&service_context), false)
        .await
        .ok();

    Ok(entities)
}

#[cfg(test)]
mod tests;
