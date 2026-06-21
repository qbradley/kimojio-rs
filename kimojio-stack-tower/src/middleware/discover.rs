// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::{Arc, Mutex};

/// Dynamic service discovery source.
pub trait Discover {
    /// Service item returned by discovery.
    type Service: Clone;

    /// Returns the current service snapshot.
    fn services(&self) -> Vec<Self::Service>;
}

/// Static discovery source backed by a vector.
#[derive(Clone, Debug)]
pub struct StaticDiscover<S> {
    services: Vec<S>,
}

/// Mutable in-memory discovery source for tests and simple dynamic setups.
#[derive(Clone, Debug)]
pub struct DynamicDiscover<S> {
    services: Arc<Mutex<Vec<S>>>,
}

impl<S> DynamicDiscover<S> {
    /// Creates a mutable discovery source.
    pub fn new(services: Vec<S>) -> Self {
        Self {
            services: Arc::new(Mutex::new(services)),
        }
    }

    /// Replaces the current service snapshot.
    pub fn replace(&self, services: Vec<S>) {
        *self
            .services
            .lock()
            .expect("dynamic discovery mutex poisoned") = services;
    }
}

impl<S> Discover for DynamicDiscover<S>
where
    S: Clone,
{
    type Service = S;

    fn services(&self) -> Vec<Self::Service> {
        self.services
            .lock()
            .expect("dynamic discovery mutex poisoned")
            .clone()
    }
}

impl<S> StaticDiscover<S> {
    /// Creates a static discovery source.
    pub fn new(services: Vec<S>) -> Self {
        Self { services }
    }
}

impl<S> Discover for StaticDiscover<S>
where
    S: Clone,
{
    type Service = S;

    fn services(&self) -> Vec<Self::Service> {
        self.services.clone()
    }
}
