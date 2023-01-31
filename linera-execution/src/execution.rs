// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    runtime::{ExecutionRuntime, SessionManager},
    system::SystemExecutionStateView,
    ApplicationId, Effect, EffectContext, ExecutionError, ExecutionResult, ExecutionRuntimeContext,
    Operation, OperationContext, Query, QueryContext, RawExecutionResult, Response, SystemEffect,
    UserApplicationId,
};
use linera_base::{data_types::ChainId, ensure};
use linera_views::{
    reentrant_collection_view::ReentrantCollectionView,
    register_view::RegisterView,
    views::{View, ViewError},
};
use linera_views_macro::HashableContainerView;

#[cfg(any(test, feature = "test"))]
use {
    crate::{system::SystemExecutionState, TestExecutionRuntimeContext},
    linera_views::{common::Context, memory::MemoryContext},
    std::collections::BTreeMap,
    std::sync::Arc,
    tokio::sync::Mutex,
};

/// A view accessing the execution state of a chain.
#[derive(Debug, HashableContainerView)]
pub struct ExecutionStateView<C> {
    /// System application.
    pub system: SystemExecutionStateView<C>,
    /// User applications.
    pub users: ReentrantCollectionView<C, UserApplicationId, RegisterView<C, Vec<u8>>>,
}

#[cfg(any(test, feature = "test"))]
impl ExecutionStateView<MemoryContext<TestExecutionRuntimeContext>>
where
    MemoryContext<TestExecutionRuntimeContext>: Context + Clone + Send + Sync + 'static,
    ViewError:
        From<<MemoryContext<TestExecutionRuntimeContext> as linera_views::common::Context>::Error>,
{
    /// Create an in-memory view where the system state is set. This is used notably to
    /// generate state hashes in tests.
    pub async fn from_system_state(state: SystemExecutionState) -> Self {
        // Destructure, to make sure we don't miss any fields.
        let SystemExecutionState {
            description,
            epoch,
            admin_id,
            subscriptions,
            committees,
            ownership,
            balance,
            timestamp,
            registry,
        } = state;
        let guard = Arc::new(Mutex::new(BTreeMap::new())).lock_owned().await;
        let extra = TestExecutionRuntimeContext::new(
            description.expect("Chain description should be set").into(),
        );
        let context = MemoryContext::new(guard, extra);
        let mut view = Self::load(context)
            .await
            .expect("Loading from memory should work");
        view.system.description.set(description);
        view.system.epoch.set(epoch);
        view.system.admin_id.set(admin_id);
        for channel_id in subscriptions {
            view.system
                .subscriptions
                .insert(&channel_id)
                .expect("serialization of channel_id should not fail");
        }
        view.system.committees.set(committees);
        view.system.ownership.set(ownership);
        view.system.balance.set(balance);
        view.system.timestamp.set(timestamp);
        view.system
            .registry
            .import(registry)
            .expect("serialization of registry components should not fail");
        view
    }
}

enum UserAction<'a> {
    Initialize(&'a OperationContext),
    Operation(&'a OperationContext, &'a [u8]),
    Effect(&'a EffectContext, &'a [u8]),
}

impl<C> ExecutionStateView<C>
where
    C: Context + Clone + Send + Sync + 'static,
    ViewError: From<C::Error>,
    C::Extra: ExecutionRuntimeContext,
{
    async fn run_user_action(
        &mut self,
        application_id: UserApplicationId,
        chain_id: ChainId,
        action: UserAction<'_>,
    ) -> Result<Vec<ExecutionResult>, ExecutionError> {
        // Try to load the application. This may fail if the corresponding
        // bytecode-publishing certificate doesn't exist yet on this validator.
        let application_description = self
            .system
            .registry
            .describe_application(application_id)
            .await?;
        let application = self
            .context()
            .extra()
            .get_user_application(&application_description)
            .await?;
        // Create the execution runtime for this transaction.
        let mut session_manager = SessionManager::default();
        let mut results = Vec::new();
        let mut application_ids = vec![application_id];
        let runtime = ExecutionRuntime::new(
            chain_id,
            &mut application_ids,
            self,
            &mut session_manager,
            &mut results,
        );
        // Make the call to user code.
        let result = match action {
            UserAction::Initialize(context) => {
                application
                    .initialize(
                        context,
                        &runtime,
                        &application_description.initialization_argument,
                    )
                    .await?
            }
            UserAction::Operation(context, operation) => {
                application
                    .execute_operation(context, &runtime, operation)
                    .await?
            }
            UserAction::Effect(context, effect) => {
                application
                    .execute_effect(context, &runtime, effect)
                    .await?
            }
        };
        assert_eq!(application_ids, vec![application_id]);
        // Make sure to declare the application first for all recipients of the user
        // execution result.
        let mut system_result = RawExecutionResult::default();
        let applications = self
            .system
            .registry
            .describe_application_with_dependencies(application_id)
            .await?;
        for effect in &result.effects {
            system_result.effects.push((
                effect.0.clone(),
                SystemEffect::RegisterApplications {
                    applications: applications.clone(),
                },
            ));
        }
        if !system_result.effects.is_empty() {
            results.push(ExecutionResult::System(system_result));
        }
        // Update externally-visible results.
        results.push(ExecutionResult::User(application_id, result));
        // Check that all sessions were properly closed.
        ensure!(
            session_manager.states.is_empty(),
            ExecutionError::SessionWasNotClosed
        );
        Ok(results)
    }

    pub async fn execute_operation(
        &mut self,
        application_id: ApplicationId,
        context: &OperationContext,
        operation: &Operation,
    ) -> Result<Vec<ExecutionResult>, ExecutionError> {
        assert_eq!(context.chain_id, self.context().extra().chain_id());
        match (application_id, operation) {
            (ApplicationId::System, Operation::System(op)) => {
                let (result, new_application) = self.system.execute_operation(context, op).await?;
                let mut results = vec![ExecutionResult::System(result)];
                if let Some(application_id) = new_application {
                    let user_action = UserAction::Initialize(context);
                    results.extend(
                        self.run_user_action(application_id, context.chain_id, user_action)
                            .await?,
                    );
                }
                Ok(results)
            }
            (ApplicationId::User(application_id), Operation::User(operation)) => {
                self.run_user_action(
                    application_id,
                    context.chain_id,
                    UserAction::Operation(context, operation),
                )
                .await
            }
            _ => Err(ExecutionError::InvalidOperation),
        }
    }

    pub async fn execute_effect(
        &mut self,
        application_id: ApplicationId,
        context: &EffectContext,
        effect: &Effect,
    ) -> Result<Vec<ExecutionResult>, ExecutionError> {
        assert_eq!(context.chain_id, self.context().extra().chain_id());
        match (application_id, effect) {
            (ApplicationId::System, Effect::System(effect)) => {
                let result = self.system.execute_effect(context, effect).await?;
                Ok(vec![ExecutionResult::System(result)])
            }
            (ApplicationId::User(application_id), Effect::User(effect)) => {
                self.run_user_action(
                    application_id,
                    context.chain_id,
                    UserAction::Effect(context, effect),
                )
                .await
            }
            _ => Err(ExecutionError::InvalidEffect),
        }
    }

    pub async fn query_application(
        &mut self,
        application_id: ApplicationId,
        context: &QueryContext,
        query: &Query,
    ) -> Result<Response, ExecutionError> {
        assert_eq!(context.chain_id, self.context().extra().chain_id());
        match (application_id, query) {
            (ApplicationId::System, Query::System(query)) => {
                let response = self.system.query_application(context, query).await?;
                Ok(Response::System(response))
            }
            (ApplicationId::User(application_id), Query::User(query)) => {
                // Load the application.
                let application_description = self
                    .system
                    .registry
                    .describe_application(application_id)
                    .await?;
                let application = self
                    .context()
                    .extra()
                    .get_user_application(&application_description)
                    .await?;
                // Create the execution runtime for this transaction.
                let mut session_manager = SessionManager::default();
                let mut results = Vec::new();
                let mut application_ids = vec![application_id];
                let runtime = ExecutionRuntime::new(
                    context.chain_id,
                    &mut application_ids,
                    self,
                    &mut session_manager,
                    &mut results,
                );
                // Run the query.
                let response = application
                    .query_application(context, &runtime, query)
                    .await?;
                assert_eq!(application_ids, vec![application_id]);
                Ok(Response::User(response))
            }
            _ => Err(ExecutionError::InvalidQuery),
        }
    }
}
