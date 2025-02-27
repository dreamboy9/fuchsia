// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.cti

use {
    crate::{
        capability::{CapabilityProvider, CapabilitySource, InternalCapability, OptionalTask},
        channel,
        config::RuntimeConfig,
        model::{
            component::{BindReason, ComponentInstance, WeakComponentInstance},
            error::ModelError,
            hooks::{Event, EventPayload, EventType, Hook, HooksRegistration},
            model::Model,
            routing::error::RoutingError,
        },
    },
    ::routing::error::ComponentInstanceError,
    anyhow::Error,
    async_trait::async_trait,
    cm_fidl_validator,
    cm_rust::{CapabilityName, FidlIntoNative},
    fidl::endpoints::ServerEnd,
    fidl_fuchsia_component as fcomponent,
    fidl_fuchsia_io::DirectoryMarker,
    fidl_fuchsia_sys2 as fsys, fuchsia_async as fasync, fuchsia_zircon as zx,
    futures::prelude::*,
    lazy_static::lazy_static,
    log::*,
    moniker::{AbsoluteMoniker, PartialMoniker},
    std::{
        cmp,
        path::PathBuf,
        sync::{Arc, Weak},
    },
};

lazy_static! {
    pub static ref REALM_SERVICE: CapabilityName = "fuchsia.sys2.Realm".into();
}

// The default implementation for framework services.
pub struct RealmCapabilityProvider {
    scope_moniker: AbsoluteMoniker,
    host: Arc<RealmCapabilityHost>,
}

impl RealmCapabilityProvider {
    pub fn new(scope_moniker: AbsoluteMoniker, host: Arc<RealmCapabilityHost>) -> Self {
        Self { scope_moniker, host }
    }
}

#[async_trait]
impl CapabilityProvider for RealmCapabilityProvider {
    async fn open(
        self: Box<Self>,
        _flags: u32,
        _open_mode: u32,
        _relative_path: PathBuf,
        server_end: &mut zx::Channel,
    ) -> Result<OptionalTask, ModelError> {
        let server_end = channel::take_channel(server_end);
        let stream = ServerEnd::<fsys::RealmMarker>::new(server_end)
            .into_stream()
            .expect("could not convert channel into stream");
        let scope_moniker = self.scope_moniker.clone();
        let host = self.host.clone();
        // We only need to look up the component matching this scope.
        // These operations should all work, even if the component is not running.
        let model = host.model.upgrade().ok_or(ModelError::ModelNotAvailable)?;
        let component = WeakComponentInstance::from(&model.look_up(&scope_moniker).await?);
        Ok(fasync::Task::spawn(async move {
            if let Err(e) = host.serve(component, stream).await {
                // TODO: Set an epitaph to indicate this was an unexpected error.
                warn!("serve failed: {}", e);
            }
        })
        .into())
    }
}

#[derive(Clone)]
pub struct RealmCapabilityHost {
    model: Weak<Model>,
    config: Arc<RuntimeConfig>,
}

// `RealmCapabilityHost` is a `Hook` that serves the `Realm` FIDL protocol.
impl RealmCapabilityHost {
    pub fn new(model: Weak<Model>, config: Arc<RuntimeConfig>) -> Self {
        Self { model, config }
    }

    pub fn hooks(self: &Arc<Self>) -> Vec<HooksRegistration> {
        vec![HooksRegistration::new(
            "RealmCapabilityHost",
            vec![EventType::CapabilityRouted],
            Arc::downgrade(self) as Weak<dyn Hook>,
        )]
    }

    pub async fn serve(
        &self,
        component: WeakComponentInstance,
        stream: fsys::RealmRequestStream,
    ) -> Result<(), fidl::Error> {
        stream
            .try_for_each_concurrent(None, |request| async {
                let method_name = request.method_name();
                let res = self.handle_request(request, &component).await;
                if let Err(e) = &res {
                    error!("Error occurred sending Realm response for {}: {}", method_name, e);
                }
                res
            })
            .await
    }

    async fn handle_request(
        &self,
        request: fsys::RealmRequest,
        component: &WeakComponentInstance,
    ) -> Result<(), fidl::Error> {
        match request {
            fsys::RealmRequest::CreateChild { responder, collection, decl } => {
                let mut res = Self::create_child(component, collection, decl).await;
                responder.send(&mut res)?;
            }
            fsys::RealmRequest::BindChild { responder, child, exposed_dir } => {
                let mut res = Self::bind_child(component, child, exposed_dir).await;
                responder.send(&mut res)?;
            }
            fsys::RealmRequest::DestroyChild { responder, child } => {
                let mut res = Self::destroy_child(component, child).await;
                responder.send(&mut res)?;
            }
            fsys::RealmRequest::ListChildren { responder, collection, iter } => {
                let mut res = Self::list_children(
                    component,
                    self.config.list_children_batch_size,
                    collection,
                    iter,
                )
                .await;
                responder.send(&mut res)?;
            }
            fsys::RealmRequest::OpenExposedDir { responder, child, exposed_dir } => {
                let mut res = Self::open_exposed_dir(component, child, exposed_dir).await;
                responder.send(&mut res)?;
            }
        }
        Ok(())
    }

    async fn create_child(
        component: &WeakComponentInstance,
        collection: fsys::CollectionRef,
        child_decl: fsys::ChildDecl,
    ) -> Result<(), fcomponent::Error> {
        let component = component.upgrade().map_err(|_| fcomponent::Error::InstanceDied)?;
        cm_fidl_validator::validate_child(&child_decl).map_err(|e| {
            error!("validate_child() failed: {}", e);
            fcomponent::Error::InvalidArguments
        })?;
        if child_decl.environment.is_some() {
            return Err(fcomponent::Error::InvalidArguments);
        }
        let child_decl = child_decl.fidl_into_native();
        component.add_dynamic_child(collection.name, &child_decl).await.map_err(|e| match e {
            ModelError::InstanceAlreadyExists { .. } => fcomponent::Error::InstanceAlreadyExists,
            ModelError::CollectionNotFound { .. } => fcomponent::Error::CollectionNotFound,
            ModelError::Unsupported { .. } => fcomponent::Error::Unsupported,
            _ => fcomponent::Error::Internal,
        })
    }

    async fn bind_child(
        component: &WeakComponentInstance,
        child: fsys::ChildRef,
        exposed_dir: ServerEnd<DirectoryMarker>,
    ) -> Result<(), fcomponent::Error> {
        match Self::get_child(component, child.clone()).await? {
            Some(child) => {
                let mut exposed_dir = exposed_dir.into_channel();
                let res = child
                    .bind(&BindReason::BindChild { parent: component.moniker.clone() })
                    .await
                    .map_err(|e| match e {
                        ModelError::ResolverError { err, .. } => {
                            debug!("failed to resolve child: {}", err);
                            fcomponent::Error::InstanceCannotResolve
                        }
                        ModelError::RunnerError { err } => {
                            debug!("failed to start child: {}", err);
                            fcomponent::Error::InstanceCannotStart
                        }
                        e => {
                            error!("bind() failed: {}", e);
                            fcomponent::Error::Internal
                        }
                    })?
                    .open_exposed(&mut exposed_dir)
                    .await;
                match res {
                    Ok(()) => (),
                    Err(ModelError::RoutingError {
                        err: RoutingError::SourceInstanceStopped { .. },
                    }) => {
                        // TODO(fxbug.dev/54109): The runner may have decided to not run the component. Perhaps a
                        // security policy prevented it, or maybe there was some other issue.
                        // Unfortunately these failed runs may or may not have occurred by this point,
                        // but we want to be consistent about how bind_child responds to these errors.
                        // Since this call succeeds if the runner hasn't yet decided to not run the
                        // component, we need to also succeed if the runner has already decided to not
                        // run the component, because otherwise the result of this call will be
                        // inconsistent.
                        ()
                    }
                    Err(e) => {
                        debug!("open_exposed() failed: {}", e);
                        return Err(fcomponent::Error::Internal);
                    }
                }
            }
            None => {
                debug!("bind_child() failed: instance not found {:?}", child);
                return Err(fcomponent::Error::InstanceNotFound);
            }
        }
        Ok(())
    }

    async fn open_exposed_dir(
        component: &WeakComponentInstance,
        child: fsys::ChildRef,
        exposed_dir: ServerEnd<DirectoryMarker>,
    ) -> Result<(), fcomponent::Error> {
        match Self::get_child(component, child.clone()).await? {
            Some(child) => {
                // Resolve child in order to instantiate exposed_dir.
                let _ = child.resolve().await.map_err(|_| {
                    return fcomponent::Error::InstanceCannotResolve;
                })?;
                let mut exposed_dir = exposed_dir.into_channel();
                let () = child.open_exposed(&mut exposed_dir).await.map_err(|e| match e {
                    ModelError::InstanceShutDown { .. } => fcomponent::Error::InstanceDied,
                    _ => {
                        debug!("open_exposed() failed: {}", e);
                        fcomponent::Error::Internal
                    }
                })?;
            }
            None => {
                debug!("open_exposed_dir() failed: instance not found {:?}", child);
                return Err(fcomponent::Error::InstanceNotFound);
            }
        }
        Ok(())
    }

    async fn destroy_child(
        component: &WeakComponentInstance,
        child: fsys::ChildRef,
    ) -> Result<(), fcomponent::Error> {
        let component = component.upgrade().map_err(|_| fcomponent::Error::InstanceDied)?;
        child.collection.as_ref().ok_or(fcomponent::Error::InvalidArguments)?;
        let partial_moniker = PartialMoniker::new(child.name, child.collection);
        let destroy_fut =
            component.remove_dynamic_child(&partial_moniker).await.map_err(|e| match e {
                ModelError::InstanceNotFoundInRealm { .. } => fcomponent::Error::InstanceNotFound,
                ModelError::Unsupported { .. } => fcomponent::Error::Unsupported,
                e => {
                    error!("remove_dynamic_child() failed: {}", e);
                    fcomponent::Error::Internal
                }
            })?;
        // This function returns as soon as the child is marked deleted, while actual destruction
        // proceeds in the background.
        fasync::Task::spawn(async move {
            let _ = destroy_fut.await;
        })
        .detach();
        Ok(())
    }

    async fn list_children(
        component: &WeakComponentInstance,
        batch_size: usize,
        collection: fsys::CollectionRef,
        iter: ServerEnd<fsys::ChildIteratorMarker>,
    ) -> Result<(), fcomponent::Error> {
        let component = component.upgrade().map_err(|_| fcomponent::Error::InstanceDied)?;
        let state = component.lock_resolved_state().await.map_err(|e| {
            error!("failed to resolve InstanceState: {}", e);
            fcomponent::Error::Internal
        })?;
        let decl = state.decl();
        let _ = decl
            .find_collection(&collection.name)
            .ok_or_else(|| fcomponent::Error::CollectionNotFound)?;
        let mut children: Vec<_> = state
            .live_children()
            .filter_map(|(m, _)| match m.collection() {
                Some(c) => {
                    if c == collection.name {
                        Some(fsys::ChildRef {
                            name: m.name().to_string(),
                            collection: m.collection().map(|s| s.to_string()),
                        })
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .collect();
        children.sort_unstable_by(|a, b| {
            let a = &a.name;
            let b = &b.name;
            if a == b {
                cmp::Ordering::Equal
            } else if a < b {
                cmp::Ordering::Less
            } else {
                cmp::Ordering::Greater
            }
        });
        let stream = iter.into_stream().map_err(|_| fcomponent::Error::AccessDenied)?;
        fasync::Task::spawn(async move {
            if let Err(e) = Self::serve_child_iterator(children, stream, batch_size).await {
                // TODO: Set an epitaph to indicate this was an unexpected error.
                warn!("serve_child_iterator failed: {}", e);
            }
        })
        .detach();
        Ok(())
    }

    async fn serve_child_iterator(
        mut children: Vec<fsys::ChildRef>,
        mut stream: fsys::ChildIteratorRequestStream,
        batch_size: usize,
    ) -> Result<(), Error> {
        while let Some(request) = stream.try_next().await? {
            match request {
                fsys::ChildIteratorRequest::Next { responder } => {
                    let n_to_send = std::cmp::min(children.len(), batch_size);
                    let mut res: Vec<_> = children.drain(..n_to_send).collect();
                    responder.send(&mut res.iter_mut())?;
                }
            }
        }
        Ok(())
    }

    async fn on_scoped_framework_capability_routed_async<'a>(
        self: Arc<Self>,
        scope_moniker: AbsoluteMoniker,
        capability: &'a InternalCapability,
        capability_provider: Option<Box<dyn CapabilityProvider>>,
    ) -> Result<Option<Box<dyn CapabilityProvider>>, ModelError> {
        // If some other capability has already been installed, then there's nothing to
        // do here.
        if capability_provider.is_none() && capability.matches_protocol(&REALM_SERVICE) {
            Ok(Some(Box::new(RealmCapabilityProvider::new(scope_moniker, self.clone()))
                as Box<dyn CapabilityProvider>))
        } else {
            Ok(capability_provider)
        }
    }

    async fn get_child(
        parent: &WeakComponentInstance,
        child: fsys::ChildRef,
    ) -> Result<Option<Arc<ComponentInstance>>, fcomponent::Error> {
        let parent = parent.upgrade().map_err(|_| fcomponent::Error::InstanceDied)?;
        let state = parent.lock_resolved_state().await.map_err(|e| match e {
            ComponentInstanceError::ResolveFailed { moniker, err, .. } => {
                debug!("failed to resolve instance with moniker {}: {}", moniker, err);
                return fcomponent::Error::InstanceCannotResolve;
            }
            e => {
                error!("failed to resolve InstanceState: {}", e);
                return fcomponent::Error::Internal;
            }
        })?;
        let partial_moniker = PartialMoniker::new(child.name, child.collection);
        Ok(state.get_live_child(&partial_moniker).map(|r| r.clone()))
    }
}

#[async_trait]
impl Hook for RealmCapabilityHost {
    async fn on(self: Arc<Self>, event: &Event) -> Result<(), ModelError> {
        if let Ok(EventPayload::CapabilityRouted {
            source: CapabilitySource::Framework { capability, component },
            capability_provider,
        }) = &event.result
        {
            let mut capability_provider = capability_provider.lock().await;
            *capability_provider = self
                .on_scoped_framework_capability_routed_async(
                    component.moniker.clone(),
                    &capability,
                    capability_provider.take(),
                )
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use {
        crate::{
            builtin_environment::BuiltinEnvironment,
            model::{
                binding::Binder,
                component::{BindReason, ComponentInstance},
                events::{registry::EventSubscription, source::EventSource, stream::EventStream},
                testing::{mocks::*, out_dir::OutDir, test_helpers::*, test_hook::*},
            },
        },
        cm_rust::{
            self, CapabilityName, CapabilityPath, ChildDecl, ComponentDecl, EventMode, ExposeDecl,
            ExposeProtocolDecl, ExposeSource, ExposeTarget, NativeIntoFidl,
        },
        cm_rust_testing::*,
        fidl::endpoints::{self, Proxy},
        fidl_fidl_examples_echo as echo,
        fidl_fuchsia_io::MODE_TYPE_SERVICE,
        fuchsia_async as fasync,
        futures::{lock::Mutex, poll, task::Poll},
        io_util::OPEN_RIGHT_READABLE,
        matches::assert_matches,
        moniker::AbsoluteMoniker,
        std::collections::HashSet,
        std::convert::TryFrom,
        std::path::PathBuf,
    };

    struct RealmCapabilityTest {
        builtin_environment: Option<Arc<Mutex<BuiltinEnvironment>>>,
        mock_runner: Arc<MockRunner>,
        component: Option<Arc<ComponentInstance>>,
        realm_proxy: fsys::RealmProxy,
        hook: Arc<TestHook>,
    }

    impl RealmCapabilityTest {
        async fn new(
            components: Vec<(&'static str, ComponentDecl)>,
            component_moniker: AbsoluteMoniker,
        ) -> Self {
            // Init model.
            let config = RuntimeConfig { list_children_batch_size: 2, ..Default::default() };
            let TestModelResult { model, builtin_environment, mock_runner, .. } =
                TestEnvironmentBuilder::new()
                    .set_components(components)
                    .set_runtime_config(config)
                    .build()
                    .await;

            let hook = Arc::new(TestHook::new());
            let hooks = hook.hooks();
            model.root.hooks.install(hooks).await;

            // Look up and bind to component.
            let component = model
                .bind(&component_moniker, &BindReason::Eager)
                .await
                .expect("failed to bind to component");

            // Host framework service.
            let (realm_proxy, stream) =
                endpoints::create_proxy_and_stream::<fsys::RealmMarker>().unwrap();
            {
                let component = WeakComponentInstance::from(&component);
                let realm_capability_host =
                    builtin_environment.lock().await.realm_capability_host.clone();
                fasync::Task::spawn(async move {
                    realm_capability_host
                        .serve(component, stream)
                        .await
                        .expect("failed serving realm service");
                })
                .detach();
            }
            RealmCapabilityTest {
                builtin_environment: Some(builtin_environment),
                mock_runner,
                component: Some(component),
                realm_proxy,
                hook,
            }
        }

        fn component(&self) -> &Arc<ComponentInstance> {
            self.component.as_ref().unwrap()
        }

        fn drop_component(&mut self) {
            self.component = None;
            self.builtin_environment = None;
        }

        async fn new_event_stream(
            &self,
            events: Vec<CapabilityName>,
            mode: EventMode,
        ) -> (EventSource, EventStream) {
            let mut event_source = self
                .builtin_environment
                .as_ref()
                .unwrap()
                .lock()
                .await
                .event_source_factory
                .create_for_debug()
                .await
                .expect("created event source");
            let event_stream = event_source
                .subscribe(
                    events
                        .into_iter()
                        .map(|event| EventSubscription::new(event, mode.clone()))
                        .collect(),
                )
                .await
                .expect("subscribe to event stream");
            event_source.start_component_tree().await;
            (event_source, event_stream)
        }
    }

    #[fuchsia::test]
    async fn create_dynamic_child() {
        // Set up model and realm service.
        let test = RealmCapabilityTest::new(
            vec![
                ("root", ComponentDeclBuilder::new().add_lazy_child("system").build()),
                ("system", ComponentDeclBuilder::new().add_transient_collection("coll").build()),
            ],
            vec!["system:0"].into(),
        )
        .await;

        let (_event_source, mut event_stream) =
            test.new_event_stream(vec![EventType::Discovered.into()], EventMode::Sync).await;

        // Create children "a" and "b" in collection. Expect a Discovered event for each.
        let mut collection_ref = fsys::CollectionRef { name: "coll".to_string() };
        let mut create_a = fasync::Task::spawn(
            test.realm_proxy.create_child(&mut collection_ref, child_decl("a")),
        );
        let event_a = event_stream
            .wait_until(EventType::Discovered, vec!["system:0", "coll:a:1"].into())
            .await
            .unwrap();
        let mut collection_ref = fsys::CollectionRef { name: "coll".to_string() };
        let mut create_b = fasync::Task::spawn(
            test.realm_proxy.create_child(&mut collection_ref, child_decl("b")),
        );
        let event_b = event_stream
            .wait_until(EventType::Discovered, vec!["system:0", "coll:b:2"].into())
            .await
            .unwrap();

        // Give create requests time to be processed. Ensure they don't return before
        // Discover action completes.
        fasync::Timer::new(fasync::Time::after(zx::Duration::from_seconds(5))).await;
        assert_matches!(poll!(&mut create_a), Poll::Pending);
        assert_matches!(poll!(&mut create_b), Poll::Pending);

        // Unblock Discovered and wait for requests to complete.
        event_a.resume();
        event_b.resume();
        let _ = create_a.await.unwrap().unwrap();
        let _ = create_b.await.unwrap().unwrap();

        // Verify that the component topology matches expectations.
        let actual_children = get_live_children(test.component()).await;
        let mut expected_children: HashSet<PartialMoniker> = HashSet::new();
        expected_children.insert("coll:a".into());
        expected_children.insert("coll:b".into());
        assert_eq!(actual_children, expected_children);
        assert_eq!("(system(coll:a,coll:b))", test.hook.print());
    }

    #[fuchsia::test]
    async fn create_dynamic_child_errors() {
        let mut test = RealmCapabilityTest::new(
            vec![
                ("root", ComponentDeclBuilder::new().add_lazy_child("system").build()),
                (
                    "system",
                    ComponentDeclBuilder::new()
                        .add_transient_collection("coll")
                        .add_collection(
                            CollectionDeclBuilder::new()
                                .name("pcoll")
                                .durability(fsys::Durability::Persistent)
                                .build(),
                        )
                        .build(),
                ),
            ],
            vec!["system:0"].into(),
        )
        .await;

        // Invalid arguments.
        {
            let mut collection_ref = fsys::CollectionRef { name: "coll".to_string() };
            let child_decl = fsys::ChildDecl {
                name: Some("a".to_string()),
                url: None,
                startup: Some(fsys::StartupMode::Lazy),
                environment: None,
                ..fsys::ChildDecl::EMPTY
            };
            let err = test
                .realm_proxy
                .create_child(&mut collection_ref, child_decl)
                .await
                .expect("fidl call failed")
                .expect_err("unexpected success");
            assert_eq!(err, fcomponent::Error::InvalidArguments);
        }
        {
            let mut collection_ref = fsys::CollectionRef { name: "coll".to_string() };
            let child_decl = fsys::ChildDecl {
                name: Some("a".to_string()),
                url: Some("test:///a".to_string()),
                startup: Some(fsys::StartupMode::Lazy),
                environment: Some("env".to_string()),
                ..fsys::ChildDecl::EMPTY
            };
            let err = test
                .realm_proxy
                .create_child(&mut collection_ref, child_decl)
                .await
                .expect("fidl call failed")
                .expect_err("unexpected success");
            assert_eq!(err, fcomponent::Error::InvalidArguments);
        }

        // Instance already exists.
        {
            let mut collection_ref = fsys::CollectionRef { name: "coll".to_string() };
            let res = test.realm_proxy.create_child(&mut collection_ref, child_decl("a")).await;
            let _ = res.expect("failed to create child a");
            let mut collection_ref = fsys::CollectionRef { name: "coll".to_string() };
            let err = test
                .realm_proxy
                .create_child(&mut collection_ref, child_decl("a"))
                .await
                .expect("fidl call failed")
                .expect_err("unexpected success");
            assert_eq!(err, fcomponent::Error::InstanceAlreadyExists);
        }

        // Collection not found.
        {
            let mut collection_ref = fsys::CollectionRef { name: "nonexistent".to_string() };
            let err = test
                .realm_proxy
                .create_child(&mut collection_ref, child_decl("a"))
                .await
                .expect("fidl call failed")
                .expect_err("unexpected success");
            assert_eq!(err, fcomponent::Error::CollectionNotFound);
        }

        // Unsupported.
        {
            let mut collection_ref = fsys::CollectionRef { name: "pcoll".to_string() };
            let err = test
                .realm_proxy
                .create_child(&mut collection_ref, child_decl("a"))
                .await
                .expect("fidl call failed")
                .expect_err("unexpected success");
            assert_eq!(err, fcomponent::Error::Unsupported);
        }
        {
            let mut collection_ref = fsys::CollectionRef { name: "coll".to_string() };
            let child_decl = fsys::ChildDecl {
                name: Some("b".to_string()),
                url: Some("test:///b".to_string()),
                startup: Some(fsys::StartupMode::Eager),
                environment: None,
                ..fsys::ChildDecl::EMPTY
            };
            let err = test
                .realm_proxy
                .create_child(&mut collection_ref, child_decl)
                .await
                .expect("fidl call failed")
                .expect_err("unexpected success");
            assert_eq!(err, fcomponent::Error::Unsupported);
        }

        // Instance died.
        {
            test.drop_component();
            let mut collection_ref = fsys::CollectionRef { name: "coll".to_string() };
            let child_decl = fsys::ChildDecl {
                name: Some("b".to_string()),
                url: Some("test:///b".to_string()),
                startup: Some(fsys::StartupMode::Lazy),
                environment: None,
                ..fsys::ChildDecl::EMPTY
            };
            let err = test
                .realm_proxy
                .create_child(&mut collection_ref, child_decl)
                .await
                .expect("fidl call failed")
                .expect_err("unexpected success");
            assert_eq!(err, fcomponent::Error::InstanceDied);
        }
    }

    #[fuchsia::test]
    async fn destroy_dynamic_child() {
        // Set up model and realm service.
        let test = RealmCapabilityTest::new(
            vec![
                ("root", ComponentDeclBuilder::new().add_lazy_child("system").build()),
                ("system", ComponentDeclBuilder::new().add_transient_collection("coll").build()),
                ("a", component_decl_with_test_runner()),
                ("b", component_decl_with_test_runner()),
            ],
            vec!["system:0"].into(),
        )
        .await;

        let (_event_source, mut event_stream) = test
            .new_event_stream(
                vec![
                    EventType::Stopped.into(),
                    EventType::MarkedForDestruction.into(),
                    EventType::Destroyed.into(),
                ],
                EventMode::Sync,
            )
            .await;

        // Create children "a" and "b" in collection, and bind to them.
        for name in &["a", "b"] {
            let mut collection_ref = fsys::CollectionRef { name: "coll".to_string() };
            let res = test.realm_proxy.create_child(&mut collection_ref, child_decl(name)).await;
            let _ = res
                .unwrap_or_else(|_| panic!("failed to create child {}", name))
                .unwrap_or_else(|_| panic!("failed to create child {}", name));
            let mut child_ref =
                fsys::ChildRef { name: name.to_string(), collection: Some("coll".to_string()) };
            let (_dir_proxy, server_end) = endpoints::create_proxy::<DirectoryMarker>().unwrap();
            let res = test.realm_proxy.bind_child(&mut child_ref, server_end).await;
            let _ = res
                .unwrap_or_else(|_| panic!("failed to bind to child {}", name))
                .unwrap_or_else(|_| panic!("failed to bind to child {}", name));
        }

        let child = get_live_child(test.component(), "coll:a").await;
        let instance_id = get_instance_id(test.component(), "coll:a").await;
        assert_eq!("(system(coll:a,coll:b))", test.hook.print());
        assert_eq!(child.component_url, "test:///a".to_string());
        assert_eq!(instance_id, 1);

        // Destroy "a". "a" is no longer live from the client's perspective, although it's still
        // being destroyed.
        let mut child_ref =
            fsys::ChildRef { name: "a".to_string(), collection: Some("coll".to_string()) };
        let (f, destroy_handle) = test.realm_proxy.destroy_child(&mut child_ref).remote_handle();
        fasync::Task::spawn(f).detach();

        // The component should be stopped (shut down) before it is marked deleted.
        let event = event_stream
            .wait_until(EventType::Stopped, vec!["system:0", "coll:a:1"].into())
            .await
            .unwrap();
        event.resume();
        let event = event_stream
            .wait_until(EventType::MarkedForDestruction, vec!["system:0", "coll:a:1"].into())
            .await
            .unwrap();

        // Child is not marked deleted yet, but should be shut down.
        {
            let actual_children = get_live_children(test.component()).await;
            let mut expected_children: HashSet<PartialMoniker> = HashSet::new();
            expected_children.insert("coll:a".into());
            expected_children.insert("coll:b".into());
            assert_eq!(actual_children, expected_children);
            let child_a = get_live_child(test.component(), "coll:a").await;
            let child_b = get_live_child(test.component(), "coll:b").await;
            assert!(execution_is_shut_down(&child_a).await);
            assert!(!execution_is_shut_down(&child_b).await);
        }

        // The destruction of "a" was arrested during `PreDestroy`. The old "a" should still exist,
        // although it's not live.
        assert!(has_child(test.component(), "coll:a:1").await);

        // Move past the 'PreDestroy' event for "a", and wait for destroy_child to return.
        event.resume();
        let res = destroy_handle.await;
        let _ = res.expect("failed to destroy child a").expect("failed to destroy child a");

        // Child is marked deleted now.
        {
            let actual_children = get_live_children(test.component()).await;
            let mut expected_children: HashSet<PartialMoniker> = HashSet::new();
            expected_children.insert("coll:b".into());
            assert_eq!(actual_children, expected_children);
            assert_eq!("(system(coll:b))", test.hook.print());
        }

        // Wait until 'PostDestroy' event for "a"
        let event = event_stream
            .wait_until(EventType::Destroyed, vec!["system:0", "coll:a:1"].into())
            .await
            .unwrap();
        event.resume();

        assert!(!has_child(test.component(), "coll:a:1").await);

        // Recreate "a" and verify "a" is back (but it's a different "a"). The old "a" is gone
        // from the client's point of view, but it hasn't been cleaned up yet.
        let mut collection_ref = fsys::CollectionRef { name: "coll".to_string() };
        let child_decl = fsys::ChildDecl {
            name: Some("a".to_string()),
            url: Some("test:///a_alt".to_string()),
            startup: Some(fsys::StartupMode::Lazy),
            environment: None,
            ..fsys::ChildDecl::EMPTY
        };
        let res = test.realm_proxy.create_child(&mut collection_ref, child_decl).await;
        let _ = res.expect("failed to recreate child a").expect("failed to recreate child a");

        assert_eq!("(system(coll:a,coll:b))", test.hook.print());
        let child = get_live_child(test.component(), "coll:a").await;
        let instance_id = get_instance_id(test.component(), "coll:a").await;
        assert_eq!(child.component_url, "test:///a_alt".to_string());
        assert_eq!(instance_id, 3);
    }

    #[fuchsia::test]
    async fn destroy_dynamic_child_errors() {
        let mut test = RealmCapabilityTest::new(
            vec![
                ("root", ComponentDeclBuilder::new().add_lazy_child("system").build()),
                ("system", ComponentDeclBuilder::new().add_transient_collection("coll").build()),
            ],
            vec!["system:0"].into(),
        )
        .await;

        // Create child "a" in collection.
        let mut collection_ref = fsys::CollectionRef { name: "coll".to_string() };
        let res = test.realm_proxy.create_child(&mut collection_ref, child_decl("a")).await;
        let _ = res.expect("failed to create child a").expect("failed to create child a");

        // Invalid arguments.
        {
            let mut child_ref = fsys::ChildRef { name: "a".to_string(), collection: None };
            let err = test
                .realm_proxy
                .destroy_child(&mut child_ref)
                .await
                .expect("fidl call failed")
                .expect_err("unexpected success");
            assert_eq!(err, fcomponent::Error::InvalidArguments);
        }

        // Instance not found.
        {
            let mut child_ref =
                fsys::ChildRef { name: "b".to_string(), collection: Some("coll".to_string()) };
            let err = test
                .realm_proxy
                .destroy_child(&mut child_ref)
                .await
                .expect("fidl call failed")
                .expect_err("unexpected success");
            assert_eq!(err, fcomponent::Error::InstanceNotFound);
        }

        // Instance died.
        {
            test.drop_component();
            let mut child_ref =
                fsys::ChildRef { name: "a".to_string(), collection: Some("coll".to_string()) };
            let err = test
                .realm_proxy
                .destroy_child(&mut child_ref)
                .await
                .expect("fidl call failed")
                .expect_err("unexpected success");
            assert_eq!(err, fcomponent::Error::InstanceDied);
        }
    }

    #[fuchsia::test]
    async fn bind_static_child() {
        // Create a hierarchy of three components, the last with eager startup. The middle
        // component hosts and exposes the "hippo" service.
        let test = RealmCapabilityTest::new(
            vec![
                ("root", ComponentDeclBuilder::new().add_lazy_child("system").build()),
                (
                    "system",
                    ComponentDeclBuilder::new()
                        .protocol(ProtocolDeclBuilder::new("foo").path("/svc/foo").build())
                        .expose(ExposeDecl::Protocol(ExposeProtocolDecl {
                            source: ExposeSource::Self_,
                            source_name: "foo".into(),
                            target_name: "hippo".into(),
                            target: ExposeTarget::Parent,
                        }))
                        .add_eager_child("eager")
                        .build(),
                ),
                ("eager", component_decl_with_test_runner()),
            ],
            vec![].into(),
        )
        .await;
        let mut out_dir = OutDir::new();
        out_dir.add_echo_service(CapabilityPath::try_from("/svc/foo").unwrap());
        test.mock_runner.add_host_fn("test:///system_resolved", out_dir.host_fn());

        // Bind to child and use exposed service.
        let mut child_ref = fsys::ChildRef { name: "system".to_string(), collection: None };
        let (dir_proxy, server_end) = endpoints::create_proxy::<DirectoryMarker>().unwrap();
        let res = test.realm_proxy.bind_child(&mut child_ref, server_end).await;
        let _ = res.expect("failed to bind to system").expect("failed to bind to system");
        let node_proxy = io_util::open_node(
            &dir_proxy,
            &PathBuf::from("hippo"),
            OPEN_RIGHT_READABLE,
            MODE_TYPE_SERVICE,
        )
        .expect("failed to open echo service");
        let echo_proxy = echo::EchoProxy::new(node_proxy.into_channel().unwrap());
        let res = echo_proxy.echo_string(Some("hippos")).await;
        assert_eq!(res.expect("failed to use echo service"), Some("hippos".to_string()));

        // Verify that the bindings happened (including the eager binding) and the component
        // topology matches expectations.
        let expected_urls =
            &["test:///root_resolved", "test:///system_resolved", "test:///eager_resolved"];
        test.mock_runner.wait_for_urls(expected_urls).await;
        assert_eq!("(system(eager))", test.hook.print());
    }

    #[fuchsia::test]
    async fn bind_dynamic_child() {
        // Create a root component with a collection and define a component that exposes a service.
        let mut out_dir = OutDir::new();
        out_dir.add_echo_service(CapabilityPath::try_from("/svc/foo").unwrap());
        let test = RealmCapabilityTest::new(
            vec![
                ("root", ComponentDeclBuilder::new().add_transient_collection("coll").build()),
                (
                    "system",
                    ComponentDeclBuilder::new()
                        .protocol(ProtocolDeclBuilder::new("foo").path("/svc/foo").build())
                        .expose(ExposeDecl::Protocol(ExposeProtocolDecl {
                            source: ExposeSource::Self_,
                            source_name: "foo".into(),
                            target_name: "hippo".into(),
                            target: ExposeTarget::Parent,
                        }))
                        .build(),
                ),
            ],
            vec![].into(),
        )
        .await;
        test.mock_runner.add_host_fn("test:///system_resolved", out_dir.host_fn());

        // Add "system" to collection.
        let mut collection_ref = fsys::CollectionRef { name: "coll".to_string() };
        let res = test.realm_proxy.create_child(&mut collection_ref, child_decl("system")).await;
        let _ = res.expect("failed to create child system").expect("failed to create child system");

        // Bind to child and use exposed service.
        let mut child_ref =
            fsys::ChildRef { name: "system".to_string(), collection: Some("coll".to_string()) };
        let (dir_proxy, server_end) = endpoints::create_proxy::<DirectoryMarker>().unwrap();
        let res = test.realm_proxy.bind_child(&mut child_ref, server_end).await;
        let _ = res.expect("failed to bind to system").expect("failed to bind to system");
        let node_proxy = io_util::open_node(
            &dir_proxy,
            &PathBuf::from("hippo"),
            OPEN_RIGHT_READABLE,
            MODE_TYPE_SERVICE,
        )
        .expect("failed to open echo service");
        let echo_proxy = echo::EchoProxy::new(node_proxy.into_channel().unwrap());
        let res = echo_proxy.echo_string(Some("hippos")).await;
        assert_eq!(res.expect("failed to use echo service"), Some("hippos".to_string()));

        // Verify that the binding happened and the component topology matches expectations.
        let expected_urls = &["test:///root_resolved", "test:///system_resolved"];
        test.mock_runner.wait_for_urls(expected_urls).await;
        assert_eq!("(coll:system)", test.hook.print());
    }

    #[fuchsia::test]
    async fn bind_child_errors() {
        let mut test = RealmCapabilityTest::new(
            vec![
                (
                    "root",
                    ComponentDeclBuilder::new()
                        .add_lazy_child("system")
                        .add_lazy_child("unresolvable")
                        .add_lazy_child("unrunnable")
                        .build(),
                ),
                ("system", component_decl_with_test_runner()),
                ("unrunnable", component_decl_with_test_runner()),
            ],
            vec![].into(),
        )
        .await;
        test.mock_runner.cause_failure("unrunnable");

        // Instance not found.
        {
            let mut child_ref = fsys::ChildRef { name: "missing".to_string(), collection: None };
            let (_, server_end) = endpoints::create_proxy::<DirectoryMarker>().unwrap();
            let err = test
                .realm_proxy
                .bind_child(&mut child_ref, server_end)
                .await
                .expect("fidl call failed")
                .expect_err("unexpected success");
            assert_eq!(err, fcomponent::Error::InstanceNotFound);
        }

        // Instance cannot resolve.
        {
            let mut child_ref =
                fsys::ChildRef { name: "unresolvable".to_string(), collection: None };
            let (_, server_end) = endpoints::create_proxy::<DirectoryMarker>().unwrap();
            let err = test
                .realm_proxy
                .bind_child(&mut child_ref, server_end)
                .await
                .expect("fidl call failed")
                .expect_err("unexpected success");
            assert_eq!(err, fcomponent::Error::InstanceCannotResolve);
        }

        // Instance died.
        {
            test.drop_component();
            let mut child_ref = fsys::ChildRef { name: "system".to_string(), collection: None };
            let (_, server_end) = endpoints::create_proxy::<DirectoryMarker>().unwrap();
            let err = test
                .realm_proxy
                .bind_child(&mut child_ref, server_end)
                .await
                .expect("fidl call failed")
                .expect_err("unexpected success");
            assert_eq!(err, fcomponent::Error::InstanceDied);
        }
    }

    // If a runner fails to launch a child, the error should not occur at `bind_child`.
    #[fuchsia::test]
    async fn bind_child_runner_failure() {
        let test = RealmCapabilityTest::new(
            vec![
                ("root", ComponentDeclBuilder::new().add_lazy_child("unrunnable").build()),
                ("unrunnable", component_decl_with_test_runner()),
            ],
            vec![].into(),
        )
        .await;
        test.mock_runner.cause_failure("unrunnable");

        let mut child_ref = fsys::ChildRef { name: "unrunnable".to_string(), collection: None };
        let (_, server_end) = endpoints::create_proxy::<DirectoryMarker>().unwrap();
        test.realm_proxy
            .bind_child(&mut child_ref, server_end)
            .await
            .expect("fidl call failed")
            .expect("bind failed");
        // TODO(fxbug.dev/46913): Assert that `server_end` closes once instance death is monitored.
    }

    fn child_decl(name: &str) -> fsys::ChildDecl {
        ChildDecl {
            name: name.to_string(),
            url: format!("test:///{}", name),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        }
        .native_into_fidl()
    }

    #[fuchsia::test]
    async fn list_children() {
        // Create a root component with collections and a static child.
        let test = RealmCapabilityTest::new(
            vec![
                (
                    "root",
                    ComponentDeclBuilder::new()
                        .add_lazy_child("static")
                        .add_transient_collection("coll")
                        .add_transient_collection("coll2")
                        .build(),
                ),
                ("static", component_decl_with_test_runner()),
            ],
            vec![].into(),
        )
        .await;

        // Create children "a" and "b" in collection 1, "c" in collection 2.
        let mut collection_ref = fsys::CollectionRef { name: "coll".to_string() };
        let res = test.realm_proxy.create_child(&mut collection_ref, child_decl("a")).await;
        let _ = res.expect("failed to create child a").expect("failed to create child a");

        let mut collection_ref = fsys::CollectionRef { name: "coll".to_string() };
        let res = test.realm_proxy.create_child(&mut collection_ref, child_decl("b")).await;
        let _ = res.expect("failed to create child b").expect("failed to create child b");

        let mut collection_ref = fsys::CollectionRef { name: "coll".to_string() };
        let res = test.realm_proxy.create_child(&mut collection_ref, child_decl("c")).await;
        let _ = res.expect("failed to create child c").expect("failed to create child c");

        let mut collection_ref = fsys::CollectionRef { name: "coll2".to_string() };
        let res = test.realm_proxy.create_child(&mut collection_ref, child_decl("d")).await;
        let _ = res.expect("failed to create child d").expect("failed to create child d");

        // Verify that we see the expected children when listing the collection.
        let (iterator_proxy, server_end) = endpoints::create_proxy().unwrap();
        let mut collection_ref = fsys::CollectionRef { name: "coll".to_string() };
        let res = test.realm_proxy.list_children(&mut collection_ref, server_end).await;
        let _ = res.expect("failed to list children").expect("failed to list children");

        let res = iterator_proxy.next().await;
        let children = res.expect("failed to iterate over children");
        assert_eq!(
            children,
            vec![
                fsys::ChildRef { name: "a".to_string(), collection: Some("coll".to_string()) },
                fsys::ChildRef { name: "b".to_string(), collection: Some("coll".to_string()) },
            ]
        );

        let res = iterator_proxy.next().await;
        let children = res.expect("failed to iterate over children");
        assert_eq!(
            children,
            vec![fsys::ChildRef { name: "c".to_string(), collection: Some("coll".to_string()) },]
        );

        let res = iterator_proxy.next().await;
        let children = res.expect("failed to iterate over children");
        assert_eq!(children, vec![]);
    }

    #[fuchsia::test]
    async fn list_children_errors() {
        // Create a root component with a collection.
        let mut test = RealmCapabilityTest::new(
            vec![("root", ComponentDeclBuilder::new().add_transient_collection("coll").build())],
            vec![].into(),
        )
        .await;

        // Collection not found.
        {
            let mut collection_ref = fsys::CollectionRef { name: "nonexistent".to_string() };
            let (_, server_end) = endpoints::create_proxy().unwrap();
            let err = test
                .realm_proxy
                .list_children(&mut collection_ref, server_end)
                .await
                .expect("fidl call failed")
                .expect_err("unexpected success");
            assert_eq!(err, fcomponent::Error::CollectionNotFound);
        }

        // Instance died.
        {
            test.drop_component();
            let mut collection_ref = fsys::CollectionRef { name: "coll".to_string() };
            let (_, server_end) = endpoints::create_proxy().unwrap();
            let err = test
                .realm_proxy
                .list_children(&mut collection_ref, server_end)
                .await
                .expect("fidl call failed")
                .expect_err("unexpected success");
            assert_eq!(err, fcomponent::Error::InstanceDied);
        }
    }

    #[fuchsia::test]
    async fn open_exposed_dir() {
        let test = RealmCapabilityTest::new(
            vec![
                ("root", ComponentDeclBuilder::new().add_lazy_child("system").build()),
                (
                    "system",
                    ComponentDeclBuilder::new()
                        .protocol(ProtocolDeclBuilder::new("foo").path("/svc/foo").build())
                        .expose(ExposeDecl::Protocol(ExposeProtocolDecl {
                            source: ExposeSource::Self_,
                            source_name: "foo".into(),
                            target_name: "hippo".into(),
                            target: ExposeTarget::Parent,
                        }))
                        .build(),
                ),
            ],
            vec![].into(),
        )
        .await;
        let (_event_source, mut event_stream) = test
            .new_event_stream(
                vec![EventType::Resolved.into(), EventType::Started.into()],
                EventMode::Async,
            )
            .await;
        let mut out_dir = OutDir::new();
        out_dir.add_echo_service(CapabilityPath::try_from("/svc/foo").unwrap());
        test.mock_runner.add_host_fn("test:///system_resolved", out_dir.host_fn());

        // Open exposed directory of child.
        let mut child_ref = fsys::ChildRef { name: "system".to_string(), collection: None };
        let (dir_proxy, server_end) = endpoints::create_proxy::<DirectoryMarker>().unwrap();
        let res = test.realm_proxy.open_exposed_dir(&mut child_ref, server_end).await;
        let _ = res.expect("open_exposed_dir() failed").expect("open_exposed_dir() failed");

        // Assert that child was resolved.
        let event = event_stream.wait_until(EventType::Resolved, vec!["system:0"].into()).await;
        assert!(event.is_some());

        // Assert that event stream doesn't have any outstanding messages.
        // This ensures that EventType::Started for "system:0" has not been
        // registered.
        let event =
            event_stream.wait_until(EventType::Started, vec!["system:0"].into()).now_or_never();
        assert!(event.is_none());

        // Now that it was asserted that "system:0" has yet to start,
        // assert that it starts after making connection below.
        let node_proxy = io_util::open_node(
            &dir_proxy,
            &PathBuf::from("hippo"),
            OPEN_RIGHT_READABLE,
            MODE_TYPE_SERVICE,
        )
        .expect("failed to open hippo service");
        let event = event_stream.wait_until(EventType::Started, vec!["system:0"].into()).await;
        assert!(event.is_some());
        let echo_proxy = echo::EchoProxy::new(node_proxy.into_channel().unwrap());
        let res = echo_proxy.echo_string(Some("hippos")).await;
        assert_eq!(res.expect("failed to use echo service"), Some("hippos".to_string()));

        // Verify topology matches expectations.
        let expected_urls = &["test:///root_resolved", "test:///system_resolved"];
        test.mock_runner.wait_for_urls(expected_urls).await;
        assert_eq!("(system)", test.hook.print());
    }

    #[fuchsia::test]
    async fn open_exposed_dir_errors() {
        let mut test = RealmCapabilityTest::new(
            vec![
                (
                    "root",
                    ComponentDeclBuilder::new()
                        .add_lazy_child("system")
                        .add_lazy_child("unresolvable")
                        .add_lazy_child("unrunnable")
                        .build(),
                ),
                ("system", component_decl_with_test_runner()),
                ("unrunnable", component_decl_with_test_runner()),
            ],
            vec![].into(),
        )
        .await;
        test.mock_runner.cause_failure("unrunnable");

        // Instance not found.
        {
            let mut child_ref = fsys::ChildRef { name: "missing".to_string(), collection: None };
            let (_, server_end) = endpoints::create_proxy::<DirectoryMarker>().unwrap();
            let err = test
                .realm_proxy
                .open_exposed_dir(&mut child_ref, server_end)
                .await
                .expect("fidl call failed")
                .expect_err("unexpected success");
            assert_eq!(err, fcomponent::Error::InstanceNotFound);
        }

        // Instance cannot resolve.
        {
            let mut child_ref =
                fsys::ChildRef { name: "unresolvable".to_string(), collection: None };
            let (_, server_end) = endpoints::create_proxy::<DirectoryMarker>().unwrap();
            let err = test
                .realm_proxy
                .open_exposed_dir(&mut child_ref, server_end)
                .await
                .expect("fidl call failed")
                .expect_err("unexpected success");
            assert_eq!(err, fcomponent::Error::InstanceCannotResolve);
        }

        // Instance can't run.
        {
            let mut child_ref = fsys::ChildRef { name: "unrunnable".to_string(), collection: None };
            let (dir_proxy, server_end) = endpoints::create_proxy::<DirectoryMarker>().unwrap();
            let res = test.realm_proxy.open_exposed_dir(&mut child_ref, server_end).await;
            let _ = res.expect("open_exposed_dir() failed").expect("open_exposed_dir() failed");
            let node_proxy = io_util::open_node(
                &dir_proxy,
                &PathBuf::from("hippo"),
                OPEN_RIGHT_READABLE,
                MODE_TYPE_SERVICE,
            )
            .expect("failed to open hippo service");
            let echo_proxy = echo::EchoProxy::new(node_proxy.into_channel().unwrap());
            let res = echo_proxy.echo_string(Some("hippos")).await;
            assert!(res.is_err());
        }

        // Instance died.
        {
            test.drop_component();
            let mut child_ref = fsys::ChildRef { name: "system".to_string(), collection: None };
            let (_, server_end) = endpoints::create_proxy::<DirectoryMarker>().unwrap();
            let err = test
                .realm_proxy
                .open_exposed_dir(&mut child_ref, server_end)
                .await
                .expect("fidl call failed")
                .expect_err("unexpected success");
            assert_eq!(err, fcomponent::Error::InstanceDied);
        }
    }
}
